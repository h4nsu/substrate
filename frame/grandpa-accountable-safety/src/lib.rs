// This file is part of Substrate.

// Copyright (C) 2021 Parity Technologies (UK) Ltd.
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

#![cfg_attr(not(feature = "std"), no_std)]

mod vote;

#[cfg(all(feature = "std", test))]
mod tests;

use codec::{self as codec, Decode, Encode};
use frame_support::traits::Get;
use sp_finality_grandpa::{
	accountable_safety::{Equivocation, Query, QueryResponse},
	AuthorityId, AuthoritySignature, Commit, RoundNumber, SetId,
};
use sp_runtime::RuntimeDebug;
use sp_std::prelude::*;

use vote::SignedVote;

pub use pallet::*;

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::pallet]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The event type of this module.
		type Event: From<Event>
			+ Into<<Self as frame_system::Config>::Event>
			+ IsType<<Self as frame_system::Config>::Event>;

		#[pallet::constant]
		type BlockTimeout: Get<Self::BlockNumber>;
	}

	#[pallet::storage]
	#[pallet::getter(fn session)]
	pub(super) type AccountableSafetySession<T: Config> =
		StorageValue<_, StoredAccountableSafetySession<T::Hash, T::BlockNumber>>;

	/// For each round we track the voters asked, and then responded.
	/// Empty Vec means that we are still waiting for a reply.
	#[pallet::storage]
	#[pallet::getter(fn queries)]
	pub(super) type AccountableSafetyQueries<T: Config> =
		StorageMap<_, Twox64Concat, AuthorityId, Query<T::Hash, T::BlockNumber>>;

	#[pallet::storage]
	#[pallet::getter(fn equivocations)]
	pub(super) type AccountableSafetyEquivocations<T: Config> =
		StorageValue<_, StoredEquivocations<T::Hash, T::BlockNumber>>;

	#[pallet::event]
	#[pallet::generate_deposit(fn deposit_event)]
	pub enum Event {
		Initiated,
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {}

	#[pallet::call]
	impl<T: Config> Pallet<T> {}
}

impl<T: Config> Pallet<T> {
	pub fn start_accountable_safety(
		block_not_included: (Commit<T::Hash, T::BlockNumber>, RoundNumber, SetId),
		new_block: (Commit<T::Hash, T::BlockNumber>, RoundNumber, SetId),
	) -> Option<()> {
		// Verify all signatures.
		if !check_commit_signatures(&block_not_included) || !check_commit_signatures(&new_block) {
			return None;
		}

		// New block that conflicts with the old block should be from a later round.
		if new_block.1 <= block_not_included.2 && block_not_included.2 > 0 {
			return None;
		}

		// WIP: It is not enough to verify signatures, the commit needs to be _validated_ that it
		// actually finalizes a conflicting block.

		// The voters in `new_block` are the ones that we will start querying, and initialize the
		// protocol with.
		let voters = new_block.0.precommits.iter().map(|commit| &commit.id);

		// The state keeps track of the block we're comparing against, and in which round the
		// protocol is at the moment.
		let state = StoredAccountableSafetySession {
			block_not_included: (block_not_included.0, block_not_included.2),
			offending_block: new_block.0.target_number,
			current_round: new_block.1,
			set_id: new_block.2,
			block_counter: 0u32.into(),
			prevote_step: false,
		};

		AccountableSafetySession::<T>::put(state);
		for voter in voters {
			AccountableSafetyQueries::<T>::insert(voter, Query::WaitingForReply);
		}

		Some(())
	}

	fn update() {
		// Early return if we don't have an active session going.
		let mut state = match Pallet::<T>::session() {
			Some(state) => state,
			None => return,
		};

		// Updating the counter assuming this is called once per block
		let block_timeout_reached = {
			state.block_counter += 1u32.into();
			AccountableSafetySession::<T>::put(state.clone());
			state.block_counter > T::BlockTimeout::get()
		};

		let all_replied = AccountableSafetyQueries::<T>::iter_values()
			.all(|query| matches!(query, Query::Replied(..)));

		// If everyone has replied, or timeout reached, move on to the next round. Otherwise return
		// early.
		if !all_replied && !block_timeout_reached {
			return;
		}

		// Group all replies before we can start checking for equivocations
		let all_prevotes = AccountableSafetyQueries::<T>::iter()
			.filter_map(|(_, reply)| match reply {
				Query::Replied(QueryResponse::Prevotes(prevotes)) => Some(prevotes),
				_ => None,
			})
			.flatten()
			.collect::<Vec<_>>();

		let mut all_precommits = if !state.prevote_step {
			AccountableSafetyQueries::<T>::iter()
				.filter_map(|(_, reply)| match reply {
					Query::Replied(QueryResponse::Precommits(precommits)) => Some(precommits),
					_ => None,
				})
				.flatten()
				.collect::<Vec<_>>()
		} else {
			Vec::new()
		};

		// If this was for the round directly after the round where the block that should have been
		// included, but wasn't, was finalized, then also check against the precommits in the commit
		// message.
		if state.current_round == state.block_not_included.1 + 1 && !state.prevote_step {
			let mut block_precommits = state.block_not_included.0.precommits.clone();
			all_precommits.append(&mut block_precommits);
		}

		// Check for equivocations and collect
		let prevote_equivocations = find_equivocations(&all_prevotes);
		let precommit_equivocations = find_equivocations(&all_precommits);
		let mut equivocations = prevote_equivocations
			.into_iter()
			.map(|v| Equivocation::Prevote(vec![v.0, v.1]))
			.chain(
				precommit_equivocations
					.into_iter()
					.map(|v| Equivocation::Precommit(vec![v.0, v.1])),
			)
			.collect::<Vec<_>>();
		let equivocations = if let Some(mut equ) = Pallet::<T>::equivocations() {
			equ.equivocations.append(&mut equivocations);
			equ
		} else {
			StoredEquivocations { equivocations }
		};
		AccountableSafetyEquivocations::<T>::put(equivocations);

		// If these were found during the prevote stage, we are done
		if state.prevote_step {
			AccountableSafetySession::<T>::kill;
			return;
		}

		// For the last round we also send out prevote queries to all voters that reported prevotes
		// instead of precommits.
		if state.current_round == state.block_not_included.1 + 1 {
			// For the authorities that replied with prevotes, proceed to the prevote query stage.
			// This means asking the voters in the commit message which prevotes they've seen
			let mut precommit_voters_in_commit = state
				.block_not_included
				.0
				.precommits
				.iter()
				.map(|precommit| precommit.id.clone())
				.collect::<Vec<_>>();
			precommit_voters_in_commit.sort_unstable();
			precommit_voters_in_commit.dedup();

			if !precommit_voters_in_commit.is_empty() {
				state.prevote_step = true;
			}
			for voter in precommit_voters_in_commit {
				AccountableSafetyQueries::<T>::insert(voter, Query::WaitingForPrevoteReply);
			}
		} else {
			// If not, proceed to the next step, which is the preceding round.
			// WIP: make this subtraction safely
			state.current_round -= 1;
			state.block_counter = 0u32.into();
			AccountableSafetySession::<T>::put(state);

			// Collect all the vote authors in the replies
			// WIP: optimization opportunity
			let mut voters = AccountableSafetyQueries::<T>::drain()
				.filter_map(|(_, reply)| match reply {
					Query::Replied(response) => Some(response.authorities().into_iter()),
					_ => None,
				})
				.flatten()
				.collect::<Vec<_>>();
			voters.sort_unstable();
			voters.dedup();

			for voter in voters {
				AccountableSafetyQueries::<T>::insert(voter, Query::WaitingForReply);
			}
		}
	}

	pub fn query_state_for_voter(voter: &AuthorityId) -> Option<Query<T::Hash, T::BlockNumber>> {
		AccountableSafetyQueries::<T>::get(voter)
	}

	pub fn add_response(
		responder: &AuthorityId,
		query_response: QueryResponse<T::Hash, T::BlockNumber>,
	) -> Option<()> {
		let state = match Pallet::<T>::session() {
			Some(state) => state,
			None => return None,
		};

		// Verify signatures of the precommits or prevotes in the response
		if !match query_response {
			QueryResponse::Precommits(ref signed_precommits) => {
				check_precommit_signatures(signed_precommits, state.current_round, state.set_id)
			}
			QueryResponse::Prevotes(ref signed_prevotes) => {
				check_prevote_signatures(signed_prevotes, state.current_round, state.set_id)
			}
		} {
			return None;
		}

		// WIP: validate the reply
		// - check that the round number is for the current one.
		// - check for equivocations in set of votes.
		// - check that it's impossible to have a supermajority for the block in question.

		// Check that the responder hasn't already responded, that the reply is the correct
		// type, and that it isn't an unsolicited response.
		// WIP: consider allowing unsolicited responses.
		match Pallet::<T>::query_state_for_voter(responder) {
			Some(Query::Replied(..)) => return None,
			Some(Query::WaitingForPrevoteReply) => {
				if !matches!(query_response, QueryResponse::Prevotes(..)) {
					return None;
				}
			}
			None => return None,
			_ => (),
		}

		AccountableSafetyQueries::<T>::insert(responder, Query::Replied(query_response));
		Some(())
	}
}

fn find_equivocations<SV, V>(votes: &Vec<SV>) -> Vec<(SV, SV)>
where
	SV: SignedVote<V, AuthorityId> + Clone,
	V: Eq,
{
	// WIP: optimization opportunity: this is O(n^2)
	let mut equivocations = Vec::new();
	let len = votes.len();
	for ii in 0..len {
		for jj in 0..len {
			if votes[ii].id() == votes[jj].id() && votes[ii].vote() != votes[jj].vote() {
				// use sp_finality_grandpa::accountable_safety::From;
				equivocations.push((votes[ii].clone(), votes[jj].clone()));
			}
		}
	}
	equivocations
}

fn check_commit_signatures<H, N>(commit_aux: &(Commit<H, N>, RoundNumber, SetId)) -> bool
where
	H: Clone + Encode,
	N: Clone + Encode,
{
	check_precommit_signatures(&commit_aux.0.precommits, commit_aux.1, commit_aux.2)
}

fn check_precommit_signatures<H, N>(
	signed_precommits: &Vec<grandpa::SignedPrecommit<H, N, AuthoritySignature, AuthorityId>>,
	round: RoundNumber,
	set_id: SetId,
) -> bool
where
	H: Clone + Encode,
	N: Clone + Encode,
{
	for signed_precommit in signed_precommits {
		if !sp_finality_grandpa::check_message_signature(
			&grandpa::Message::Precommit(signed_precommit.precommit.clone()),
			&signed_precommit.id,
			&signed_precommit.signature,
			round,
			set_id,
		) {
			return false;
		}
	}
	true
}

fn check_prevote_signatures<H, N>(
	signed_prevotes: &Vec<grandpa::SignedPrevote<H, N, AuthoritySignature, AuthorityId>>,
	round: RoundNumber,
	set_id: SetId,
) -> bool
where
	H: Clone + Encode,
	N: Clone + Encode,
{
	for signed_prevote in signed_prevotes {
		if !sp_finality_grandpa::check_message_signature(
			&grandpa::Message::Prevote(signed_prevote.prevote.clone()),
			&signed_prevote.id,
			&signed_prevote.signature,
			round,
			set_id,
		) {
			return false;
		}
	}
	true
}

#[derive(Clone, RuntimeDebug, Encode, Decode, PartialEq, Eq)]
pub struct StoredAccountableSafetySession<H, N> {
	/// Earlier block not included.
	pub block_not_included: (sp_finality_grandpa::Commit<H, N>, RoundNumber),
	/// Latter block, for which the first block isn't an ancestor.
	pub offending_block: N,
	/// The current round in the protocol. We start from the round of the offending block and walk
	/// backwards to the round after the round where the block not included was finalized.
	pub current_round: RoundNumber,
	/// The set for the current round.
	pub set_id: SetId,
	/// Keep track on the number of elapsed blocks, to make sure we can timeout if needed.
	pub block_counter: N,
	/// If we are in the prevote step
	pub prevote_step: bool,
}

#[derive(Clone, RuntimeDebug, Encode, Decode)]
pub struct StoredEquivocations<H, N> {
	pub equivocations: Vec<Equivocation<H, N>>,
}
