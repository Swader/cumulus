// This file is part of Substrate.

// Copyright (C) 2019-2021 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Asset Transaction Payment Module
//!
//! This module provides the basic logic needed to pay the absolute minimum amount needed for a
//! transaction to be included via the assets (other than the main token of the chain).

#![cfg_attr(not(feature = "std"), no_std)]

use sp_std::prelude::*;
use codec::{Encode, Decode, EncodeLike};
use frame_support::{
	decl_storage, decl_module,
	DefaultNoBound,
	traits::Get,
	weights::{
		Weight, DispatchInfo, PostDispatchInfo, GetDispatchInfo, Pays, WeightToFeePolynomial,
		WeightToFeeCoefficient, DispatchClass,
	},
	dispatch::DispatchResult,
};
use sp_runtime::{
	FixedU128, FixedPointNumber, FixedPointOperand, Perquintill, RuntimeDebug,
	transaction_validity::{
		InvalidTransaction, TransactionPriority, ValidTransaction, TransactionValidityError, TransactionValidity,
	},
	traits::{
		Saturating, SignedExtension, SaturatedConversion, Convert, Dispatchable,
		DispatchInfoOf, PostDispatchInfoOf, Zero, One,
	},
};
use pallet_assets::BalanceConversion;
use pallet_balances::NegativeImbalance;
use pallet_transaction_payment::OnChargeTransaction;
use frame_support::traits::tokens::{fungibles::{Balanced, Inspect, CreditOf}, WithdrawConsequence};

#[cfg(test)]
mod tests;

type BalanceOf<T> = <<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::Balance;
type AssetBalanceOf<T> = <<T as Config>::Fungibles as Inspect<<T as frame_system::Config>::AccountId>>::Balance;
type AssetIdOf<T> = <<T as Config>::Fungibles as Inspect<<T as frame_system::Config>::AccountId>>::AssetId;
type LiquidityInfoOf<T> = <<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::LiquidityInfo;

#[derive(Encode, Decode, DefaultNoBound)]
pub enum InitialPayment<T: Config> {
	Nothing,
	Native(LiquidityInfoOf<T>),
	Asset(CreditOf<T::AccountId, T::Fungibles>),
}

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	use frame_support::{
		dispatch::DispatchResultWithPostInfo,
		pallet_prelude::*,
		inherent::Vec,
		traits::{
			Currency, ReservableCurrency, EnsureOrigin, ExistenceRequirement::KeepAlive,
		},
		PalletId,
	};
	use frame_system::pallet_prelude::*;

	#[pallet::config]
	pub trait Config: frame_system::Config + pallet_transaction_payment::Config + pallet_balances::Config + pallet_authorship::Config + pallet_assets::Config {
		type BalanceConversion: BalanceConversion<BalanceOf<Self>, AssetIdOf<Self>, AssetBalanceOf<Self>>;
		type Fungibles: Balanced<Self::AccountId>;
	}

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {}
}

impl<T: Config> Pallet<T> where
	BalanceOf<T>: FixedPointOperand + Into<AssetBalanceOf<T>>,
	AssetBalanceOf<T>: FixedPointOperand,
{

}

/// Require the transactor pay for themselves and maybe include a tip to gain additional priority
/// in the queue. Allows paying via both `Currency` as well as `fungibles::Balanced`.
#[derive(Encode, Decode, Clone, Eq, PartialEq)]
pub struct ChargeAssetTxPayment<T: Config>(#[codec(compact)] BalanceOf<T>, Option<AssetIdOf<T>>);

impl<T: Config> ChargeAssetTxPayment<T> where
	T::Call: Dispatchable<Info=DispatchInfo, PostInfo=PostDispatchInfo>,
	BalanceOf<T>: Send + Sync + FixedPointOperand + Into<AssetBalanceOf<T>>,
	AssetIdOf<T>: Send + Sync,
	AssetBalanceOf<T>: Send + Sync + FixedPointOperand,
{
	/// utility constructor. Used only in client/factory code.
	pub fn from(fee: BalanceOf<T>, asset_id: Option<AssetIdOf<T>>) -> Self {
		Self(fee, asset_id)
	}

	fn withdraw_fee(
		&self,
		who: &T::AccountId,
		call: &T::Call,
		info: &DispatchInfoOf<T::Call>,
		len: usize,
	) -> Result<
		(
			BalanceOf<T>,
			InitialPayment<T>,
		),
		TransactionValidityError,
	> {
		let tip = self.0;
		let fee = pallet_transaction_payment::Module::<T>::compute_fee(len as u32, info, tip);

		if fee.is_zero() {
			return Ok((fee, InitialPayment::Nothing));
		}

		let maybe_asset_id  = self.1;
		if let Some(asset_id) = maybe_asset_id {
			let converted_fee = T::BalanceConversion::to_asset_balance(fee, asset_id)
				.map_err(|_| -> TransactionValidityError { InvalidTransaction::Payment.into() })?;
			let can_withdraw = <T::Fungibles as Inspect<T::AccountId>>::can_withdraw(asset_id, who, converted_fee);
			if !matches!(can_withdraw, WithdrawConsequence::Success) {
				return Err(InvalidTransaction::Payment.into());
			}
			<T::Fungibles as Balanced<T::AccountId>>::withdraw(asset_id, who, converted_fee)
				.map(|i| (fee, InitialPayment::Asset(i)))
				.map_err(|_| -> TransactionValidityError { InvalidTransaction::Payment.into() })
		} else {
			<<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::withdraw_fee(who, call, info, fee, tip)
				.map(|i| (fee, InitialPayment::Native(i)))
				.map_err(|_| -> TransactionValidityError { InvalidTransaction::Payment.into() })
		}
	}

	/// Get an appropriate priority for a transaction with the given length and info.
	///
	/// This will try and optimise the `fee/weight` `fee/length`, whichever is consuming more of the
	/// maximum corresponding limit.
	///
	/// For example, if a transaction consumed 1/4th of the block length and half of the weight, its
	/// final priority is `fee * min(2, 4) = fee * 2`. If it consumed `1/4th` of the block length
	/// and the entire block weight `(1/1)`, its priority is `fee * min(1, 4) = fee * 1`. This means
	///  that the transaction which consumes more resources (either length or weight) with the same
	/// `fee` ends up having lower priority.
	fn get_priority(len: usize, info: &DispatchInfoOf<T::Call>, final_fee: BalanceOf<T>) -> TransactionPriority {
		let weight_saturation = T::BlockWeights::get().max_block / info.weight.max(1);
		let max_block_length = *T::BlockLength::get().max.get(DispatchClass::Normal);
		let len_saturation = max_block_length as u64 / (len as u64).max(1);
		let coefficient: BalanceOf<T> = weight_saturation.min(len_saturation).saturated_into::<BalanceOf<T>>();
		final_fee.saturating_mul(coefficient).saturated_into::<TransactionPriority>()
	}

	fn correct_and_deposit_fee(
		who: &T::AccountId,
		_dispatch_info: &DispatchInfoOf<T::Call>,
		_post_info: &PostDispatchInfoOf<T::Call>,
		corrected_fee: BalanceOf<T>,
		tip: BalanceOf<T>,
		paid: CreditOf<T::AccountId, T::Fungibles>,
	) -> Result<(), TransactionValidityError> {
		let converted_fee = T::BalanceConversion::to_asset_balance(corrected_fee, paid.asset())
		.map_err(|_| -> TransactionValidityError { InvalidTransaction::Payment.into() })?;
		// Calculate how much refund we should return
		let (refund, final_fee) = paid.split(converted_fee);
		// refund to the the account that paid the fees. If this fails, the
		// account might have dropped below the existential balance. In
		// that case we don't refund anything.
		// TODO: what to do in case this errors?
		let _res = <T::Fungibles as Balanced<T::AccountId>>::resolve(who, refund);

		let author = pallet_authorship::Module::<T>::author();
		// TODO: what to do in case paying the author fails (e.g. because `fee < min_balance`)
		<T::Fungibles as Balanced<T::AccountId>>::resolve(&author, final_fee)
			.map_err(|_| -> TransactionValidityError { InvalidTransaction::Payment.into() })?;
		Ok(())
	}
}

impl<T: Config> sp_std::fmt::Debug for ChargeAssetTxPayment<T>
{
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		write!(f, "ChargeAssetTxPayment<{:?}, {:?}>", self.0, self.1.encode())
	}
	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		Ok(())
	}
}

impl<T: Config> SignedExtension for ChargeAssetTxPayment<T> where
	BalanceOf<T>: Send + Sync + From<u64> + FixedPointOperand + Into<AssetBalanceOf<T>>,
	T::Call: Dispatchable<Info=DispatchInfo, PostInfo=PostDispatchInfo>,
	AssetIdOf<T>: Send + Sync,
	AssetBalanceOf<T>: Send + Sync + FixedPointOperand,
{
	const IDENTIFIER: &'static str = "ChargeAssetTxPayment";
	type AccountId = T::AccountId;
	type Call = T::Call;
	type AdditionalSigned = ();
	type Pre = (
		// tip
		BalanceOf<T>,
		// who paid the fee
		Self::AccountId,
		// imbalance resulting from withdrawing the fee
		InitialPayment<T>,
	);
	fn additional_signed(&self) -> sp_std::result::Result<(), TransactionValidityError> { Ok(()) }

	fn validate(
		&self,
		who: &Self::AccountId,
		call: &Self::Call,
		info: &DispatchInfoOf<Self::Call>,
		len: usize,
	) -> TransactionValidity {
		let (fee, _) = self.withdraw_fee(who, call, info, len)?;
		Ok(ValidTransaction {
			priority: Self::get_priority(len, info, fee),
			..Default::default()
		})
	}

	fn pre_dispatch(
		self,
		who: &Self::AccountId,
		call: &Self::Call,
		info: &DispatchInfoOf<Self::Call>,
		len: usize
	) -> Result<Self::Pre, TransactionValidityError> {
		let (_fee, initial_payment) = self.withdraw_fee(who, call, info, len)?;
		Ok((self.0, who.clone(), initial_payment))
	}

	fn post_dispatch(
		pre: Self::Pre,
		info: &DispatchInfoOf<Self::Call>,
		post_info: &PostDispatchInfoOf<Self::Call>,
		len: usize,
		_result: &DispatchResult,
	) -> Result<(), TransactionValidityError> {
		let (tip, who, initial_payment) = pre;
		let actual_fee = pallet_transaction_payment::Module::<T>::compute_actual_fee(
			len as u32,
			info,
			post_info,
			tip,
		);
		match initial_payment {
			InitialPayment::Native(imbalance) => {
				<<T as pallet_transaction_payment::Config>::OnChargeTransaction as OnChargeTransaction<T>>::correct_and_deposit_fee(&who, info, post_info, actual_fee, tip, imbalance)?;
			},
			InitialPayment::Asset(credit) => {
				Self::correct_and_deposit_fee(&who, info, post_info, actual_fee, tip, credit)?;
			},
			// TODO: just assert that actual_fee is also zero?
			InitialPayment::Nothing => {
				debug_assert!(actual_fee.is_zero(), "actual fee should be zero if initial fee was zero.");
			},
		}

		Ok(())
	}
}