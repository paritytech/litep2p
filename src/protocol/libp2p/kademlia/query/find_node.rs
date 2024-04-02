// Copyright 2023 litep2p developers
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use crate::{
	protocol::libp2p::kademlia::{
		message::KademliaMessage,
		query::{QueryAction, QueryId},
		types::{Distance, KademliaPeer, Key},
	},
	PeerId,
};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::query::find_node";

/// Context for `FIND_NODE` queries.
#[derive(Debug)]
pub struct FindNodeContext<T: Clone + Into<Vec<u8>>> {
	/// Local peer ID.
	local_peer_id: PeerId,

	/// Query ID.
	pub query: QueryId,

	/// Target key.
	pub target: Key<T>,

	/// Peers from whom the `QueryEngine` is waiting to hear a response.
	pub pending: HashMap<PeerId, KademliaPeer>,

	/// Queried candidates.
	///
	/// These are the peers for whom the query has already been sent
	/// and who have either returned their closest peers or failed to answer.
	pub queried: HashSet<PeerId>,

	/// Candidates.
	pub candidates: BTreeMap<Distance, KademliaPeer>,

	/// Responses.
	pub responses: BTreeMap<Distance, KademliaPeer>,

	/// Replication factor.
	pub replication_factor: usize,

	/// Parallelism factor.
	pub parallelism_factor: usize,
}

impl<T: Clone + Into<Vec<u8>>> FindNodeContext<T> {
	/// Create new [`FindNodeContext`].
	pub fn new(
		local_peer_id: PeerId,
		query: QueryId,
		target: Key<T>,
		in_peers: VecDeque<KademliaPeer>,
		replication_factor: usize,
		parallelism_factor: usize,
	) -> Self {
		let mut candidates = BTreeMap::new();

		for candidate in &in_peers {
			let distance = target.distance(&candidate.key);
			candidates.insert(distance, candidate.clone());
		}

		Self {
			query,
			target,
			candidates,
			local_peer_id,
			pending: HashMap::new(),
			queried: HashSet::new(),
			responses: BTreeMap::new(),
			replication_factor,
			parallelism_factor,
		}
	}

	/// Register response failure for `peer`.
	pub fn register_response_failure(&mut self, peer: PeerId) {
		let Some(peer) = self.pending.remove(&peer) else {
			tracing::debug!(target: LOG_TARGET, ?peer, "pending peer doesn't exist");
			return;
		};

		self.queried.insert(peer.peer);
	}

	/// Register `FIND_NODE` response from `peer`.
	pub fn register_response(&mut self, peer: PeerId, peers: Vec<KademliaPeer>) {
		let Some(peer) = self.pending.remove(&peer) else {
			tracing::warn!(target: LOG_TARGET, ?peer, "received response from peer but didn't expect it");
			debug_assert!(false);
			return;
		};

		// calculate distance for `peer` from target and insert it if
		//  a) the map doesn't have 20 responses
		//  b) it can replace some other peer that has a higher distance
		let distance = self.target.distance(&peer.key);

		// always mark the peer as queried to prevent it getting queried again
		self.queried.insert(peer.peer);

		// TODO: could this be written in another way?
		// TODO: only insert nodes from whom a response was received
		match self.responses.len() < self.replication_factor {
			true => {
				self.responses.insert(distance, peer);
			},
			false => {
				let mut entry = self.responses.last_entry().expect("entry to exist");
				if entry.key() > &distance {
					entry.insert(peer);
				}
			},
		}

		// filter already queried peers and extend the set of candidates
		for candidate in peers {
			if !self.queried.contains(&candidate.peer) &&
				!self.pending.contains_key(&candidate.peer)
			{
				if self.local_peer_id == candidate.peer {
					continue;
				}

				let distance = self.target.distance(&candidate.key);
				self.candidates.insert(distance, candidate);
			}
		}
	}

	/// Get next action for `peer`.
	pub fn next_peer_action(&mut self, peer: &PeerId) -> Option<QueryAction> {
		self.pending.contains_key(peer).then_some(QueryAction::SendMessage {
			query: self.query,
			peer: *peer,
			message: KademliaMessage::find_node(self.target.clone().into_preimage()),
		})
	}

	/// Schedule next peer for outbound `FIND_NODE` query.
	pub fn schedule_next_peer(&mut self) -> QueryAction {
		tracing::trace!(target: LOG_TARGET, query = ?self.query, "get next peer");

		let (_, candidate) = self.candidates.pop_first().expect("entry to exist");
		self.pending.insert(candidate.peer, candidate.clone());

		QueryAction::SendMessage {
			query: self.query,
			peer: candidate.peer,
			message: KademliaMessage::find_node(self.target.clone().into_preimage()),
		}
	}

	/// Get next action for a `FIND_NODE` query.
	// TODO: refactor this function
	pub fn next_action(&mut self) -> Option<QueryAction> {
		// we didn't receive any responses and there are no candidates or pending queries left.
		if self.responses.is_empty() && self.pending.is_empty() && self.candidates.is_empty() {
			return Some(QueryAction::QueryFailed { query: self.query });
		}

		// there are still possible peers to query or peers who are being queried
		if self.responses.len() < self.replication_factor &&
			(!self.pending.is_empty() || !self.candidates.is_empty())
		{
			if self.pending.len() == self.parallelism_factor || self.candidates.is_empty() {
				return None;
			}

			return Some(self.schedule_next_peer());
		}

		// query succeeded with one or more results
		if self.pending.is_empty() && self.candidates.is_empty() {
			return Some(QueryAction::QuerySucceeded { query: self.query });
		}

		// check if any candidate has lower distance thant the current worst
		// `expect()` is ok because both `candidates` and `responses` have been confirmed to contain
		// entries
		if !self.candidates.is_empty() {
			let first_candidate_distance = self
				.target
				.distance(&self.candidates.first_key_value().expect("candidate to exist").1.key);
			let worst_response_candidate =
				self.responses.last_entry().expect("response to exist").key().clone();

			if first_candidate_distance < worst_response_candidate &&
				self.pending.len() < self.parallelism_factor
			{
				return Some(self.schedule_next_peer());
			}

			return Some(QueryAction::QuerySucceeded { query: self.query });
		}

		if self.responses.len() == self.replication_factor {
			return Some(QueryAction::QuerySucceeded { query: self.query });
		}

		tracing::error!(
			target: LOG_TARGET,
			candidates_len = ?self.candidates.len(),
			pending_len = ?self.pending.len(),
			responses_len = ?self.responses.len(),
			"unhandled state"
		);

		unreachable!();
	}
}

// TODO: tests
