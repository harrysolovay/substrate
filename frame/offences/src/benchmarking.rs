// This file is part of Substrate.

// Copyright (C) 2020-2022 Parity Technologies (UK) Ltd.
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

//! Offences pallet benchmarking.

#![cfg(feature = "runtime-benchmarks")]

use super::*;
use sp_std::{prelude::*, vec};

use frame_benchmarking::{account, benchmarks};
use frame_support::traits::{Currency, Get, ValidatorSet, ValidatorSetWithIdentification};
use frame_system::{Config as SystemConfig, Pallet as System, RawOrigin};

use sp_runtime::{
	traits::{Convert, Saturating, StaticLookup, UniqueSaturatedInto},
	Perbill,
};
use sp_staking::offence::{Offence, ReportOffence};

use pallet_balances::Config as BalancesConfig;
use pallet_grandpa::{GrandpaEquivocationOffence, GrandpaTimeSlot};
use pallet_im_online::{Config as ImOnlineConfig, Pallet as ImOnline, UnresponsivenessOffence};
use crate::{Config as OffencesConfig, Pallet as Offences};
use pallet_session::{
	historical::{Config as HistoricalConfig, IdentificationTuple},
	Config as SessionConfig, SessionManager,
};
use pallet_staking::{
	Config as StakingConfig, Event as StakingEvent, Exposure, IndividualExposure,
	Pallet as Staking, RewardDestination, ValidatorPrefs,
};

const SEED: u32 = 0;

const MAX_REPORTERS: u32 = 100;
const MAX_OFFENDERS: u32 = 100;
const MAX_NOMINATORS: u32 = 100;

pub struct Pallet<T: Config>(Offences<T>);

pub trait Config:
	SessionConfig
	+ StakingConfig
	+ OffencesConfig
	+ ImOnlineConfig
	+ HistoricalConfig
	+ BalancesConfig
	+ IdTupleConvert<Self>
{
}

/// A helper trait to make sure we can convert `IdentificationTuple` coming from historical
/// and the one required by offences.
pub trait IdTupleConvert<T: HistoricalConfig + OffencesConfig> {
	/// Convert identification tuple from `historical` trait to the one expected by `offences`.
	fn convert(id: IdentificationTuple<T>) -> <T as OffencesConfig>::IdentificationTuple;
}

impl<T: HistoricalConfig + OffencesConfig> IdTupleConvert<T> for T
where
	<T as OffencesConfig>::IdentificationTuple: From<IdentificationTuple<T>>,
{
	fn convert(id: IdentificationTuple<T>) -> <T as OffencesConfig>::IdentificationTuple {
		id.into()
	}
}

type LookupSourceOf<T> = <<T as SystemConfig>::Lookup as StaticLookup>::Source;
type BalanceOf<T> =
	<<T as StakingConfig>::Currency as Currency<<T as SystemConfig>::AccountId>>::Balance;

struct Offender<T: Config> {
	pub controller: T::AccountId,
	pub stash: T::AccountId,
	pub nominator_stashes: Vec<T::AccountId>,
}

fn bond_amount<T: Config>() -> BalanceOf<T> {
	T::Currency::minimum_balance().saturating_mul(10_000u32.into())
}

fn create_offender<T: Config>(n: u32, nominators: u32) -> Result<Offender<T>, &'static str> {
	let stash: T::AccountId = account("stash", n, SEED);
	let controller: T::AccountId = account("controller", n, SEED);
	let controller_lookup: LookupSourceOf<T> = T::Lookup::unlookup(controller.clone());
	let reward_destination = RewardDestination::Staked;
	let raw_amount = bond_amount::<T>();
	// add twice as much balance to prevent the account from being killed.
	let free_amount = raw_amount.saturating_mul(2u32.into());
	T::Currency::make_free_balance_be(&stash, free_amount);
	let amount: BalanceOf<T> = raw_amount.into();
	Staking::<T>::bond(
		RawOrigin::Signed(stash.clone()).into(),
		controller_lookup.clone(),
		amount.clone(),
		reward_destination.clone(),
	)?;

	let validator_prefs =
		ValidatorPrefs { commission: Perbill::from_percent(50), ..Default::default() };
	Staking::<T>::validate(RawOrigin::Signed(controller.clone()).into(), validator_prefs)?;

	let mut individual_exposures = vec![];
	let mut nominator_stashes = vec![];
	// Create n nominators
	for i in 0..nominators {
		let nominator_stash: T::AccountId =
			account("nominator stash", n * MAX_NOMINATORS + i, SEED);
		let nominator_controller: T::AccountId =
			account("nominator controller", n * MAX_NOMINATORS + i, SEED);
		let nominator_controller_lookup: LookupSourceOf<T> =
			T::Lookup::unlookup(nominator_controller.clone());
		T::Currency::make_free_balance_be(&nominator_stash, free_amount.into());

		Staking::<T>::bond(
			RawOrigin::Signed(nominator_stash.clone()).into(),
			nominator_controller_lookup.clone(),
			amount.clone(),
			reward_destination.clone(),
		)?;

		let selected_validators: Vec<LookupSourceOf<T>> = vec![controller_lookup.clone()];
		Staking::<T>::nominate(
			RawOrigin::Signed(nominator_controller.clone()).into(),
			selected_validators,
		)?;

		individual_exposures
			.push(IndividualExposure { who: nominator_stash.clone(), value: amount.clone() });
		nominator_stashes.push(nominator_stash.clone());
	}

	let exposure =
		Exposure { total: amount.clone() * n.into(), own: amount, others: individual_exposures };
	let current_era = 0u32;
	Staking::<T>::add_era_stakers(current_era.into(), stash.clone().into(), exposure);

	Ok(Offender { controller, stash, nominator_stashes })
}

fn make_offenders<T: Config>(
	num_offenders: u32,
	num_nominators: u32,
) -> Result<(Vec<IdentificationTuple<T>>, Vec<Offender<T>>), &'static str> {
	Staking::<T>::new_session(0);

	let mut offenders = vec![];
	for i in 0..num_offenders {
		let offender = create_offender::<T>(i + 1, num_nominators)?;
		offenders.push(offender);
	}

	Staking::<T>::start_session(0);

	let id_tuples = offenders
		.iter()
		.map(|offender| {
			<T as SessionConfig>::ValidatorIdOf::convert(offender.controller.clone())
				.expect("failed to get validator id from account id")
		})
		.map(|validator_id| {
			<T as HistoricalConfig>::FullIdentificationOf::convert(validator_id.clone())
				.map(|full_id| (validator_id, full_id))
				.expect("failed to convert validator id to full identification")
		})
		.collect::<Vec<IdentificationTuple<T>>>();
	Ok((id_tuples, offenders))
}

benchmarks! {
	report_offence_without_hook {
		let n in 0 .. MAX_NOMINATORS.min(<T as pallet_staking::Config>::MaxNominations::get());

		// for babe equivocation reports the number of reporters
		// and offenders is always 1
		let reporters = vec![account("reporter", 1, SEED)];

		// make sure reporters actually get rewarded
		Staking::<T>::set_slash_reward_fraction(Perbill::one());

		let (mut offenders, raw_offenders) = make_offenders::<T>(1, n)?;
		let keys =  ImOnline::<T>::keys();

		let offence = MockOffence {
			slot: 0u64.into(),
			session_index: 0,
			validator_set_count: keys.len() as u32,
			offender: T::convert(offenders.pop().unwrap()),
		};
		assert_eq!(System::<T>::event_count(), 0);
	}: {
		let _ = Offences::<T>::report_offence(reporters, offence);
	}
	verify {
		#[cfg(test)]
		{
			// make sure that all slashes have been applied
			assert_eq!(
				System::<T>::event_count(), 0
				+ 1 // offence
				+ 3 // reporter (reward + endowment)
				+ 2 // offenders slashed
				+ 1 // offenders chilled
				+ 2 * n // nominators slashed
			);
		}
	}

	impl_benchmark_test_suite!(Pallet, crate::bench_mock::new_test_ext(), crate::bench_mock::Test);
}

// Basically a copy of the `BabeEquivocationOffence` without depending on the babe pallet.
pub struct MockOffence<Offender> {
	/// A babe slot in which this incident happened.
	pub slot: u64,
	/// The session index in which the incident happened.
	pub session_index: SessionIndex,
	/// The size of the validator set at the time of the offence.
	pub validator_set_count: u32,
	/// The authority that produced the equivocation.
	pub offender: Offender,
}

impl<Offender: Clone> sp_staking::offence::Offence<Offender>
	for MockOffence<Offender>
{
	const ID: Kind = *b"mock:equivocatio";
	type TimeSlot = u64;

	fn offenders(&self) -> Vec<Offender> {
		vec![self.offender.clone()]
	}

	fn session_index(&self) -> SessionIndex {
		self.session_index
	}

	fn validator_set_count(&self) -> u32 {
		self.validator_set_count
	}

	fn time_slot(&self) -> Self::TimeSlot {
		self.slot
	}

	fn slash_fraction(offenders_count: u32, validator_set_count: u32) -> Perbill {
		// the formula is min((3k / n)^2, 1)
		let x = Perbill::from_rational(3 * offenders_count, validator_set_count);
		// _ ^ 2
		x.square()
	}
}
