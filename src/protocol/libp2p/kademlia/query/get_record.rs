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
        record::{Key as RecordKey, PeerRecord, Record},
        types::{Distance, KademliaPeer, Key},
        Quorum,
    },
    PeerId,
};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::query::get_record";

/// The configuration needed to instantiate a new [`GetRecordContext`].
#[derive(Debug)]
pub struct GetRecordConfig {
    /// Local peer ID.
    pub local_peer_id: PeerId,

    /// How many records we already know about (ie extracted from storage).
    pub record_count: usize,

    /// Quorum for the query.
    pub quorum: Quorum,

    /// Replication factor.
    pub replication_factor: usize,

    /// Parallelism factor.
    pub parallelism_factor: usize,

    /// Query ID.
    pub query: QueryId,

    /// Target key.
    pub target: Key<RecordKey>,
}

#[derive(Debug)]
pub struct GetRecordContext {
    /// Query immutable config.
    pub config: GetRecordConfig,

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
    pub found_records: Vec<PeerRecord>,
}

impl GetRecordContext {
    /// Create new [`GetRecordContext`].
    pub fn new(config: GetRecordConfig, in_peers: VecDeque<KademliaPeer>) -> Self {
        let mut candidates = BTreeMap::new();

        for candidate in &in_peers {
            let distance = config.target.distance(&candidate.key);
            candidates.insert(distance, candidate.clone());
        }

        Self {
            config,
            candidates,
            pending: HashMap::new(),
            queried: HashSet::new(),
            found_records: Vec::new(),
        }
    }

    /// Get the found records.
    pub fn found_records(mut self) -> Vec<PeerRecord> {
        self.found_records
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

        if let Some(record) = record {
            if !record.is_expired(std::time::Instant::now()) {
                self.found_records.push(PeerRecord {
                    peer: peer.peer,
                    record,
                });
            }
        }

        // add the queried peer to `queried` and all new peers which haven't been
        // queried to `candidates`
        self.queried.insert(peer.peer);

        let to_query_candidate = peers.into_iter().filter_map(|peer| {
            // Peer already produced a response.
            if self.queried.contains(&peer.peer) {
                return None;
            }

            // Peer was queried, awaiting response.
            if self.pending.contains_key(&peer.peer) {
                return None;
            }

            // Local node.
            if self.config.local_peer_id == peer.peer {
                return None;
            }

            Some(peer)
        });

        for candidate in to_query_candidate {
            let distance = self.config.target.distance(&candidate.key);
            self.candidates.insert(distance, candidate);
        }
    }

    /// Get next action for `peer`.
    // TODO: remove this and store the next action to `PeerAction`
    pub fn next_peer_action(&mut self, peer: &PeerId) -> Option<QueryAction> {
        self.pending.contains_key(peer).then_some(QueryAction::SendMessage {
            query: self.config.query,
            peer: *peer,
            message: KademliaMessage::get_record(self.config.target.clone().into_preimage()),
        })
    }

    /// Schedule next peer for outbound `GET_VALUE` query.
    fn schedule_next_peer(&mut self) -> Option<QueryAction> {
        tracing::trace!(target: LOG_TARGET, query = ?self.config.query, "get next peer");

        let Some((_, candidate)) = self.candidates.pop_first() else {
            return None;
        };

        let peer = candidate.peer;

        tracing::trace!(target: LOG_TARGET, ?peer, "current candidate");
        self.pending.insert(candidate.peer, candidate);

        Some(QueryAction::SendMessage {
            query: self.config.query,
            peer,
            message: KademliaMessage::get_record(self.config.target.clone().into_preimage()),
        })
    }

    /// Get next action for a `GET_VALUE` query.
    pub fn next_action(&mut self) -> Option<QueryAction> {
        // if there are no more peers to query, check if the query succeeded or failed
        // the status is determined by whether a record was found
        if self.pending.is_empty() && self.candidates.is_empty() {
            match self.config.record_count + self.found_records.len() {
                0 =>
                    return Some(QueryAction::QueryFailed {
                        query: self.config.query,
                    }),
                _ =>
                    return Some(QueryAction::QuerySucceeded {
                        query: self.config.query,
                    }),
            }
        }

        // check if enough records have been found
        let continue_search = match self.config.quorum {
            Quorum::All =>
                (self.config.record_count + self.found_records.len()
                    < self.config.replication_factor),
            Quorum::One => (self.config.record_count + self.found_records.len() < 1),
            Quorum::N(num_responses) =>
                (self.config.record_count + self.found_records.len() < num_responses.into()),
        };

        // if enough replicas for the record have been received (defined by the quorum size),
        /// mark the query as succeeded
        if !continue_search {
            return Some(QueryAction::QuerySucceeded {
                query: self.config.query,
            });
        }

        // if the search must continue, try to schedule next outbound message if possible
        if !self.pending.is_empty() || !self.candidates.is_empty() {
            if self.pending.len() == self.config.parallelism_factor || self.candidates.is_empty() {
                return None;
            }

            return self.schedule_next_peer();
        }

        // TODO: probably not correct
        tracing::warn!(
            target: LOG_TARGET,
            num_pending = ?self.pending.len(),
            num_candidates = ?self.candidates.len(),
            num_records = ?(self.config.record_count + self.found_records.len()),
            quorum = ?self.config.quorum,
            ?continue_search,
            "unreachable condition for `GET_VALUE` search"
        );

        unreachable!();
    }
}
