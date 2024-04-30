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

use bytes::Bytes;

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

impl GetRecordConfig {
    /// Checks if the found number of records meets the specified quorum.
    ///
    /// Used to determine if the query found enough records to stop.
    fn sufficient_records(&self, records: usize) -> bool {
        // The total number of known records is the sum of the records we knew about before starting
        // the query and the records we found along the way.
        let total_known = self.record_count + records;

        match self.quorum {
            Quorum::All => total_known >= self.replication_factor,
            Quorum::One => total_known >= 1,
            Quorum::N(needed_responses) => total_known >= needed_responses.get(),
        }
    }
}

#[derive(Debug)]
pub struct GetRecordContext {
    /// Query immutable config.
    pub config: GetRecordConfig,

    /// Cached Kadmelia message to send.
    kad_message: Bytes,

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

        let kad_message = KademliaMessage::get_record(config.target.clone().into_preimage());

        Self {
            config,
            kad_message,

            candidates,
            pending: HashMap::new(),
            queried: HashSet::new(),
            found_records: Vec::new(),
        }
    }

    /// Get the found records.
    pub fn found_records(self) -> Vec<PeerRecord> {
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

        // Add the queried peer to `queried` and all new peers which haven't been
        // queried to `candidates`
        self.queried.insert(peer.peer);

        if let Some(record) = record {
            if !record.is_expired(std::time::Instant::now()) {
                self.found_records.push(PeerRecord {
                    peer: peer.peer,
                    record,
                });
            }
        }

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
            message: self.kad_message.clone(),
        })
    }

    /// Schedule next peer for outbound `GET_VALUE` query.
    fn schedule_next_peer(&mut self) -> Option<QueryAction> {
        tracing::trace!(target: LOG_TARGET, query = ?self.config.query, "get next peer");

        let (_, candidate) = self.candidates.pop_first()?;

        let peer = candidate.peer;

        tracing::trace!(target: LOG_TARGET, ?peer, "current candidate");
        self.pending.insert(candidate.peer, candidate);

        Some(QueryAction::SendMessage {
            query: self.config.query,
            peer,
            message: self.kad_message.clone(),
        })
    }

    /// Check if the query cannot make any progress.
    ///
    /// Returns true when there are no pending responses and no candidates to query.
    fn is_done(&self) -> bool {
        self.pending.is_empty() && self.candidates.is_empty()
    }

    /// Get next action for a `GET_VALUE` query.
    pub fn next_action(&mut self) -> Option<QueryAction> {
        // These are the records we knew about before starting the query and
        // the records we found along the way.
        let known_records = self.config.record_count + self.found_records.len();

        // If we cannot make progress, return the final result.
        // A query failed when we are not able to identify one single record.
        if self.is_done() {
            return if known_records == 0 {
                Some(QueryAction::QueryFailed {
                    query: self.config.query,
                })
            } else {
                Some(QueryAction::QuerySucceeded {
                    query: self.config.query,
                })
            };
        }

        // Check if enough records have been found
        let sufficient_records = self.config.sufficient_records(self.found_records.len());
        if sufficient_records {
            return Some(QueryAction::QuerySucceeded {
                query: self.config.query,
            });
        }

        // At this point, we either have pending responses or candidates to query; and we need more
        // records. Ensure we do not exceed the parallelism factor.
        if self.pending.len() == self.config.parallelism_factor {
            return None;
        }

        self.schedule_next_peer()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_config() -> GetRecordConfig {
        GetRecordConfig {
            local_peer_id: PeerId::random(),
            quorum: Quorum::All,
            record_count: 0,
            replication_factor: 20,
            parallelism_factor: 10,
            query: QueryId(0),
            target: Key::new(vec![1, 2, 3].into()),
        }
    }

    #[test]
    fn config_check() {
        // Quorum::All with no known records.
        let config = GetRecordConfig {
            quorum: Quorum::All,
            record_count: 0,
            replication_factor: 20,
            ..default_config()
        };
        assert!(config.sufficient_records(20));
        assert!(!config.sufficient_records(19));

        // Quorum::All with 10 known records.
        let config = GetRecordConfig {
            quorum: Quorum::All,
            record_count: 10,
            replication_factor: 20,
            ..default_config()
        };
        assert!(config.sufficient_records(10));
        assert!(!config.sufficient_records(9));

        // Quorum::One with no known records.
        let config = GetRecordConfig {
            quorum: Quorum::One,
            record_count: 0,
            ..default_config()
        };
        assert!(config.sufficient_records(1));
        assert!(!config.sufficient_records(0));

        // Quorum::One with known records.
        let config = GetRecordConfig {
            quorum: Quorum::One,
            record_count: 10,
            ..default_config()
        };
        assert!(config.sufficient_records(1));
        assert!(config.sufficient_records(0));

        // Quorum::N with no known records.
        let config = GetRecordConfig {
            quorum: Quorum::N(std::num::NonZeroUsize::new(10).expect("valid; qed")),
            record_count: 0,
            ..default_config()
        };
        assert!(config.sufficient_records(10));
        assert!(!config.sufficient_records(9));

        // Quorum::N with known records.
        let config = GetRecordConfig {
            quorum: Quorum::N(std::num::NonZeroUsize::new(10).expect("valid; qed")),
            record_count: 5,
            ..default_config()
        };
        assert!(config.sufficient_records(5));
        assert!(!config.sufficient_records(4));
    }

    #[test]
    fn completes_when_no_candidates() {
        let config = default_config();
        let mut context = GetRecordContext::new(config, VecDeque::new());
        assert!(context.is_done());
        let event = context.next_action().unwrap();
        assert_eq!(event, QueryAction::QueryFailed { query: QueryId(0) });

        let config = GetRecordConfig {
            record_count: 1,
            ..default_config()
        };
        let mut context = GetRecordContext::new(config, VecDeque::new());
        assert!(context.is_done());
        let event = context.next_action().unwrap();
        assert_eq!(event, QueryAction::QuerySucceeded { query: QueryId(0) });
    }
}
