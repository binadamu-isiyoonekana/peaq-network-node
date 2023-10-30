// This file is part of Acala.

// Copyright (C) 2020-2023 Acala Foundation.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

//! # Evm Accounts Module
//!
//! ## Overview
//!
//! Evm Accounts module provide a two way mapping between Substrate accounts and
//! EVM accounts so user only have deal with one account / private key.

#![cfg_attr(not(feature = "std"), no_std)]
#![allow(clippy::unused_unit)]

use frame_support::{
	ensure,
	pallet_prelude::*,
	traits::{
		fungible, fungible::Inspect, Currency, ExistenceRequirement, IsType, OnKilledAccount,
	},
	transactional,
};
use frame_system::{ensure_signed, pallet_prelude::*};
use pallet_evm::AddressMapping as PalletEVMAddressMapping;
use parity_scale_codec::Encode;
use precompile_utils::prelude::keccak256;

use peaq_primitives_xcm::{evm::EvmAddress, to_bytes};
use sp_core::{crypto::AccountId32, H160, H256};
use sp_io::{crypto::secp256k1_ecdsa_recover, hashing::keccak_256};
use sp_runtime::traits::{Convert, Zero};
use sp_std::{marker::PhantomData, vec::Vec};

mod convert_impl;
mod mock;
mod tests;
mod traits;
pub mod weights;

use convert_impl::*;
pub use module::*;
pub use traits::EVMAddressMapping;
pub use weights::WeightInfo;

/// A signature (a 512-bit value, plus 8 bits for recovery ID).
pub type Eip712Signature = [u8; 65];
type AccountIdOf<T> = <T as frame_system::Config>::AccountId;
type BalanceOf<T> = <<T as Config>::Currency as Currency<AccountIdOf<T>>>::Balance;

#[frame_support::pallet]
pub mod module {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The Currency for managing Evm account assets.
		type Currency: Currency<Self::AccountId>
			+ fungible::Inspect<Self::AccountId, Balance = BalanceOf<Self>>;

		type OriginAddressMapping: PalletEVMAddressMapping<Self::AccountId>;
		/*
		 *         /// Mapping from address to account id.
		 *         type AddressMapping: EVMAddressMapping<Self::AccountId>;
		 *
		 */
		/// Chain ID of EVM.
		#[pallet::constant]
		type ChainId: Get<u64>;

		/// Weight information for the extrinsics in this module.
		type WeightInfo: WeightInfo;
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(crate) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Mapping between Substrate accounts and EVM accounts
		/// claim account.
		ClaimAccount { account_id: T::AccountId, evm_address: EvmAddress },
	}

	/// Error for evm accounts module.
	#[pallet::error]
	pub enum Error<T> {
		/// AccountId has mapped
		AccountIdHasMapped,
		/// Eth address has mapped
		EthAddressHasMapped,
		/// Bad signature
		BadSignature,
		/// Invalid signature
		InvalidSignature,
		/// Account ref count is not zero
		NonZeroRefCount,
	}

	/// The Substrate Account for EvmAddresses
	///
	/// Accounts: map EvmAddress => Option<AccountId>
	#[pallet::storage]
	#[pallet::getter(fn accounts)]
	pub type Accounts<T: Config> =
		StorageMap<_, Twox64Concat, EvmAddress, T::AccountId, OptionQuery>;

	/// The EvmAddress for Substrate Accounts
	///
	/// EvmAddresses: map AccountId => Option<EvmAddress>
	#[pallet::storage]
	#[pallet::getter(fn evm_addresses)]
	pub type EvmAddresses<T: Config> =
		StorageMap<_, Twox64Concat, T::AccountId, EvmAddress, OptionQuery>;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<T::BlockNumber> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Claim account mapping between Substrate accounts and EVM accounts.
		/// Ensure eth_address has not been mapped.
		///
		/// - `eth_address`: The address to bind to the caller's account
		/// - `eth_signature`: A signature generated by the address to prove ownership
		// Link
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::claim_account())]
		#[transactional]
		pub fn claim_account(
			origin: OriginFor<T>,
			eth_address: EvmAddress,
			eth_signature: Eip712Signature,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;

			// ensure account_id and eth_address has not been mapped
			ensure!(!EvmAddresses::<T>::contains_key(&who), Error::<T>::AccountIdHasMapped);
			ensure!(!Accounts::<T>::contains_key(eth_address), Error::<T>::EthAddressHasMapped);

			// recover evm address from signature
			let address = Self::verify_eip712_signature(&who, &eth_signature)
				.ok_or(Error::<T>::BadSignature)?;
			ensure!(eth_address == address, Error::<T>::InvalidSignature);

			let account_id = T::OriginAddressMapping::into_account_id(eth_address);
			if frame_system::Pallet::<T>::account_exists(&account_id) {
				// merge balance from `evm padded address` to `origin`
				let amount = T::Currency::reducible_balance(&account_id, false);
				T::Currency::transfer(&account_id, &who, amount, ExistenceRequirement::AllowDeath)?;
			}

			Accounts::<T>::insert(eth_address, &who);
			EvmAddresses::<T>::insert(&who, eth_address);

			Self::deposit_event(Event::ClaimAccount { account_id: who, evm_address: eth_address });

			Ok(())
		}
	}
}

impl<T: Config> Pallet<T> {
	#[cfg(any(feature = "runtime-benchmarks", feature = "std"))]
	// Returns an Ethereum public key derived from an Ethereum secret key.
	pub fn eth_public(secret: &libsecp256k1::SecretKey) -> libsecp256k1::PublicKey {
		libsecp256k1::PublicKey::from_secret_key(secret)
	}

	#[cfg(any(feature = "runtime-benchmarks", feature = "std"))]
	// Returns an Ethereum address derived from an Ethereum secret key.
	// Only for tests
	pub fn eth_address(secret: &libsecp256k1::SecretKey) -> EvmAddress {
		EvmAddress::from_slice(&keccak_256(&Self::eth_public(secret).serialize()[1..65])[12..])
	}

	#[cfg(any(feature = "runtime-benchmarks", feature = "std"))]
	// Constructs a message and signs it.
	pub fn eth_sign(secret: &libsecp256k1::SecretKey, who: &T::AccountId) -> Eip712Signature {
		let msg = keccak_256(&Self::eip712_signable_message(who));
		let (sig, recovery_id) = libsecp256k1::sign(&libsecp256k1::Message::parse(&msg), secret);
		let mut r = [0u8; 65];
		r[0..64].copy_from_slice(&sig.serialize()[..]);
		r[64] = recovery_id.serialize();
		r
	}

	fn verify_eip712_signature(who: &T::AccountId, sig: &[u8; 65]) -> Option<H160> {
		let msg = Self::eip712_signable_message(who);
		let msg_hash = keccak_256(msg.as_slice());

		recover_signer(sig, &msg_hash)
	}

	// Eip-712 message to be signed
	fn eip712_signable_message(who: &T::AccountId) -> Vec<u8> {
		let domain_separator = Self::evm_account_domain_separator();
		let payload_hash = Self::evm_account_payload_hash(who);

		let mut msg = b"\x19\x01".to_vec();
		msg.extend_from_slice(&domain_separator);
		msg.extend_from_slice(&payload_hash);
		msg
	}

	fn evm_account_payload_hash(who: &T::AccountId) -> [u8; 32] {
		let tx_type_hash = keccak256!("Transaction(bytes substrateAddress)");
		let mut tx_msg = tx_type_hash.to_vec();
		tx_msg.extend_from_slice(&keccak_256(&who.encode()));
		keccak_256(tx_msg.as_slice())
	}

	fn evm_account_domain_separator() -> [u8; 32] {
		let domain_hash =
			keccak256!("EIP712Domain(string name,string version,uint256 chainId,bytes32 salt)");
		let mut domain_seperator_msg = domain_hash.to_vec();
		domain_seperator_msg.extend_from_slice(&keccak256!("Peaq EVM claim")); // name
		domain_seperator_msg.extend_from_slice(&keccak256!("1")); // version
		domain_seperator_msg.extend_from_slice(&to_bytes(T::ChainId::get())); // chain id
		domain_seperator_msg.extend_from_slice(
			frame_system::Pallet::<T>::block_hash(T::BlockNumber::zero()).as_ref(),
		); // genesis block hash
		keccak_256(domain_seperator_msg.as_slice())
	}
}

fn recover_signer(sig: &[u8; 65], msg_hash: &[u8; 32]) -> Option<H160> {
	secp256k1_ecdsa_recover(sig, msg_hash)
		.map(|pubkey| H160::from(H256::from_slice(&keccak_256(&pubkey))))
		.ok()
}

impl<T: Config> PalletEVMAddressMapping<T::AccountId> for Pallet<T>
where
	T::AccountId: IsType<AccountId32>,
	T::OriginAddressMapping: PalletEVMAddressMapping<T::AccountId>,
{
	fn into_account_id(address: EvmAddress) -> T::AccountId {
		EVMAddressToAccountId::<T>::convert(address)
	}
}

impl<T: Config> EVMAddressMapping<T::AccountId> for Pallet<T>
where
	T::AccountId: IsType<AccountId32>,
	T::OriginAddressMapping: PalletEVMAddressMapping<T::AccountId>,
{
	// Returns the AccountId used to generate the given EvmAddress.
	fn get_account_id(address: &EvmAddress) -> T::AccountId {
		Self::into_account_id(*address)
	}

	// Returns the EvmAddress associated with a given AccountId or the
	// underlying EvmAddress of the AccountId.
	// Returns None if there is no EvmAddress associated with the AccountId
	// and there is no underlying EvmAddress in the AccountId.
	fn get_evm_address(account_id: &T::AccountId) -> Option<EvmAddress> {
		AccountIdToEVMAddress::<T>::convert(account_id.clone())
	}

	// Returns true if a given AccountId is associated with a given EvmAddress
	// and false if is not.
	// Note: we don't check whether the default EvmAddress of the AccountId is linked or not
	fn is_linked(account_id: &T::AccountId, evm: &EvmAddress) -> bool {
		Self::get_evm_address(account_id).as_ref() == Some(evm)
	}
}

pub struct CallKillEVMLinkAccount<T>(PhantomData<T>);
impl<T: Config> OnKilledAccount<T::AccountId> for CallKillEVMLinkAccount<T> {
	fn on_killed_account(who: &T::AccountId) {
		// remove mapping created by `claim_account` or `get_or_create_evm_address`
		if let Some(evm_addr) = Pallet::<T>::evm_addresses(who) {
			Accounts::<T>::remove(evm_addr);
			EvmAddresses::<T>::remove(who);
		}
	}
}

/*
 * // TODO, Need to survey
 * // I guess it is related to the address unification, but let us survey it later
 * impl<T: Config> StaticLookup for Pallet<T> {
 *     type Source = MultiAddress<T::AccountId, AccountIndex>;
 *     type Target = T::AccountId;
 *
 *     fn lookup(a: Self::Source) -> Result<Self::Target, LookupError> {
 *         match a {
 *             MultiAddress::Address20(i) =>
 * Ok(T::AddressMapping::get_account_id(&EvmAddress::from_slice(&i))),             _ =>
 * Err(LookupError),         }
 *     }
 *
 *     fn unlookup(a: Self::Target) -> Self::Source {
 *         MultiAddress::Id(a)
 *     }
 * }
 */
