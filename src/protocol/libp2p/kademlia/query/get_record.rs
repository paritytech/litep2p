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

#![allow(unused)]

use crate::{
    protocol::libp2p::kademlia::{
        message::KademliaMessage,
        query::{QueryAction, QueryId},
        record::{Key as RecordKey, Record},
        types::{Distance, KademliaPeer, Key},
        Quorum,
    },
    PeerId,
};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::query::get_record";

#[derive(Debug)]
pub struct GetRecordContext {
    /// Local peer ID.
    local_peer_id: PeerId,

    /// How many records have been successfully found.
    pub record_count: usize,

    /// Quorum for the query.
    pub quorum: Quorum,

    /// Query ID.
    pub query: QueryId,

    /// Target key.
    pub target: Key<RecordKey>,

    /// Peers from whom the `QueryEngine` is waiting to hear a response.
    pub pending: HashMap<PeerId, KademliaPeer>,

    /// Queried candidates.
    ///
    /// These are the peers for whom the query has already been sent
    /// and who have either returned their closest peers or failed to answer.
    pub queried: HashSet<PeerId>,

    /// Candidates.
    pub candidates: BTreeMap<Distance, KademliaPeer>,

    /// Found records.
    pub found_records: Vec<Record>,

    /// Replication factor.
    pub replication_factor: usize,

    /// Parallelism factor.
    pub parallelism_factor: usize,
}

impl GetRecordContext {
    /// Create new [`GetRecordContext`].
    pub fn new(
        local_peer_id: PeerId,
        query: QueryId,
        target: Key<RecordKey>,
        in_peers: VecDeque<KademliaPeer>,
        replication_factor: usize,
        parallelism_factor: usize,
        quorum: Quorum,
        record_count: usize,
    ) -> Self {
        let mut candidates = BTreeMap::new();

        for candidate in &in_peers {
            let distance = target.distance(&candidate.key);
            candidates.insert(distance, candidate.clone());
        }

        Self {
            query,
            target,
            quorum,
            candidates,
            record_count,
            local_peer_id,
            replication_factor,
            parallelism_factor,
            pending: HashMap::new(),
            queried: HashSet::new(),
            found_records: Vec::new(),
        }
    }

    /// Get the found record.
    pub fn found_record(mut self) -> Record {
        self.found_records.pop().expect("record to exist since query succeeded")
    }

    /// Register response failure for `peer`.
    pub fn register_response_failure(&mut self, peer: PeerId) {
        let Some(peer) = self.pending.remove(&peer) else {
            tracing::trace!(target: LOG_TARGET, ?peer, "pending peer doesn't exist");
            return;
        };

        self.queried.insert(peer.peer);
    }

    /// Register `GET_VALUE` response from `peer`.
    pub fn register_response(
        &mut self,
        peer: PeerId,
        record: Option<Record>,
        peers: Vec<KademliaPeer>,
    ) {
        let Some(peer) = self.pending.remove(&peer) else {
            tracing::trace!(target: LOG_TARGET, ?peer, "received response from peer but didn't expect it");
            return;
        };

        // TODO: validate record
        if let Some(record) = record {
            self.found_records.push(record);
        }

        // add the queried peer to `queried` and all new peers which haven't been
        // queried to `candidates`
        self.queried.insert(peer.peer);

        for candidate in peers {
            if !self.queried.contains(&candidate.peer)
                && !self.pending.contains_key(&candidate.peer)
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
    // TODO: remove this and store the next action to `PeerAction`
    pub fn next_peer_action(&mut self, peer: &PeerId) -> Option<QueryAction> {
        self.pending.contains_key(peer).then_some(QueryAction::SendMessage {
            query: self.query,
            peer: *peer,
            message: KademliaMessage::get_record(self.target.clone().into_preimage()),
        })
    }

    /// Schedule next peer for outbound `GET_VALUE` query.
    pub fn schedule_next_peer(&mut self) -> QueryAction {
        tracing::trace!(target: LOG_TARGET, query = ?self.query, "get next peer");

        let (_, candidate) = self.candidates.pop_first().expect("entry to exist");
        let peer = candidate.peer;

        tracing::trace!(target: LOG_TARGET, ?peer, "current candidate");
        self.pending.insert(candidate.peer, candidate);

        QueryAction::SendMessage {
            query: self.query,
            peer,
            message: KademliaMessage::get_record(self.target.clone().into_preimage()),
        }
    }

    /// Get next action for a `GET_VALUE` query.
    pub fn next_action(&mut self) -> Option<QueryAction> {
        // if there are no more peers to query, check if the query succeeded or failed
        // the status is determined by whether a record was found
        if self.pending.is_empty() && self.candidates.is_empty() {
            match self.record_count + self.found_records.len() {
                0 => return Some(QueryAction::QueryFailed { query: self.query }),
                _ => return Some(QueryAction::QuerySucceeded { query: self.query }),
            }
        }

        // check if enough records have been found
        let continue_search = match self.quorum {
            Quorum::All => (self.record_count + self.found_records.len() < self.replication_factor),
            Quorum::One => (self.record_count + self.found_records.len() < 1),
            Quorum::N(num_responses) =>
                (self.record_count + self.found_records.len() < num_responses.into()),
        };

        // if enough replicas for the record have been received (defined by the quorum size),
        /// mark the query as succeeded
        if !continue_search {
            return Some(QueryAction::QuerySucceeded { query: self.query });
        }

        // if the search must continue, try to schedule next outbound message if possible
        if !self.pending.is_empty() || !self.candidates.is_empty() {
            if self.pending.len() == self.parallelism_factor || self.candidates.is_empty() {
                return None;
            }

            return Some(self.schedule_next_peer());
        }

        // TODO: probably not correct
        tracing::warn!(
            target: LOG_TARGET,
            num_pending = ?self.pending.len(),
            num_candidates = ?self.candidates.len(),
            num_records = ?(self.record_count + self.found_records.len()),
            quorum = ?self.quorum,
            ?continue_search,
            "unreachable condition for `GET_VALUE` search"
        );

        unreachable!();
    }
}
