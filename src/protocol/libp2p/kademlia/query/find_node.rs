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

use std::collections::{BTreeMap, HashMap, VecDeque};

/// Logging target for the file.
const LOG_TARGET: &str = "ipfs::kademlia::query::find_node";

/// Context for `FIND_NODE` queries.
#[derive(Debug)]
pub struct FindNodeContext<T: Clone + Into<Vec<u8>>> {
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
    pub queried: HashMap<PeerId, KademliaPeer>,

    /// Candidates.
    pub candidates: VecDeque<KademliaPeer>,

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
        query: QueryId,
        target: Key<T>,
        candidates: VecDeque<KademliaPeer>,
        replication_factor: usize,
        parallelism_factor: usize,
    ) -> Self {
        Self {
            query,
            target,
            candidates,
            pending: HashMap::new(),
            queried: HashMap::new(),
            responses: BTreeMap::new(),
            replication_factor,
            parallelism_factor,
        }
    }

    /// Register response failure for `peer`.
    pub fn register_response_failure(&mut self, peer: PeerId) {
        let Some(peer) = self.pending.remove(&peer) else {
            tracing::warn!(target: LOG_TARGET, ?peer, "pending peer doesn't exist");
            debug_assert!(false);
            return;
        };

        self.queried.insert(peer.peer, peer);
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

        // TODO: could this be written in another way?
        // TODO: only insert nodes from whom a response was received
        match self.responses.len() < self.replication_factor {
            true => {
                self.responses.insert(distance, peer);
            }
            false => {
                let mut entry = self.responses.last_entry().expect("entry to exist");
                if entry.key() > &distance {
                    entry.insert(peer);
                }
            }
        }

        self.candidates.extend(peers.clone());
        self.candidates.make_contiguous().sort_by(|a, b| {
            self.target
                .distance(&a.key)
                .cmp(&self.target.distance(&b.key))
        });
    }

    /// Get next action for `peer`.
    pub fn next_peer_action(&mut self, peer: &PeerId) -> Option<QueryAction> {
        self.pending
            .contains_key(peer)
            .then_some(QueryAction::SendMessage {
                query: self.query,
                peer: *peer,
                message: KademliaMessage::find_node(self.target.clone().into_preimage()),
            })
    }

    /// Schedule next peer for outbound `FIND_NODE` query.
    pub fn schedule_next_peer(&mut self) -> QueryAction {
        tracing::trace!(target: LOG_TARGET, query = ?self.query, "get next peer");

        let candidate = self.candidates.pop_front().expect("entry to exist");
        tracing::trace!(target: LOG_TARGET, ?candidate, "current candidate");
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
        if self.responses.len() < self.replication_factor
            && (!self.pending.is_empty() || !self.candidates.is_empty())
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
        if !self.candidates.is_empty() {
            let first_candidate_distance = self.target.distance(&self.candidates[0].key);
            let worst_response_candidate = self.responses.last_entry().unwrap().key().clone();

            if first_candidate_distance < worst_response_candidate {
                return Some(self.schedule_next_peer());
            }

            // TODO: this is probably not correct
            return Some(QueryAction::QueryFailed { query: self.query });
        }

        // TODO: probably not correct
        unreachable!();
    }
}

// TODO: tests
