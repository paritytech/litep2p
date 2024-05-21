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

/// The configuration needed to instantiate a new [`FindNodeContext`].
#[derive(Debug)]
pub struct FindNodeConfig<T: Clone + Into<Vec<u8>>> {
    /// Local peer ID.
    pub local_peer_id: PeerId,

    /// Replication factor.
    pub replication_factor: usize,

    /// Parallelism factor.
    pub parallelism_factor: usize,

    /// Query ID.
    pub query: QueryId,

    /// Target key.
    pub target: Key<T>,
}

/// Context for `FIND_NODE` queries.
#[derive(Debug)]
pub struct FindNodeContext<T: Clone + Into<Vec<u8>>> {
    /// Query immutable config.
    pub config: FindNodeConfig<T>,

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
}

impl<T: Clone + Into<Vec<u8>>> FindNodeContext<T> {
    /// Create new [`FindNodeContext`].
    pub fn new(config: FindNodeConfig<T>, in_peers: VecDeque<KademliaPeer>) -> Self {
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
            responses: BTreeMap::new(),
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
        let distance = self.config.target.distance(&peer.key);

        // always mark the peer as queried to prevent it getting queried again
        self.queried.insert(peer.peer);

        if self.responses.len() < self.config.replication_factor {
            self.responses.insert(distance, peer);
        } else {
            // Update the last entry if the peer is closer.
            if let Some(mut entry) = self.responses.last_entry() {
                if entry.key() > &distance {
                    entry.insert(peer);
                }
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
    pub fn next_peer_action(&mut self, peer: &PeerId) -> Option<QueryAction> {
        self.pending.contains_key(peer).then_some(QueryAction::SendMessage {
            query: self.config.query,
            peer: *peer,
            message: KademliaMessage::find_node(self.config.target.clone().into_preimage()),
        })
    }

    /// Schedule next peer for outbound `FIND_NODE` query.
    fn schedule_next_peer(&mut self) -> Option<QueryAction> {
        tracing::trace!(target: LOG_TARGET, query = ?self.config.query, "get next peer");

        let (_, candidate) = self.candidates.pop_first()?;

        self.pending.insert(candidate.peer, candidate.clone());

        Some(QueryAction::SendMessage {
            query: self.config.query,
            peer: candidate.peer,
            message: KademliaMessage::find_node(self.config.target.clone().into_preimage()),
        })
    }

    /// Get next action for a `FIND_NODE` query.
    // TODO: refactor this function
    pub fn next_action(&mut self) -> Option<QueryAction> {
        // we didn't receive any responses and there are no candidates or pending queries left.
        if self.responses.is_empty() && self.pending.is_empty() && self.candidates.is_empty() {
            return Some(QueryAction::QueryFailed {
                query: self.config.query,
            });
        }

        // there are still possible peers to query or peers who are being queried
        if self.responses.len() < self.config.replication_factor
            && (!self.pending.is_empty() || !self.candidates.is_empty())
        {
            if self.pending.len() == self.config.parallelism_factor || self.candidates.is_empty() {
                return None;
            }

            return self.schedule_next_peer();
        }

        // query succeeded with one or more results
        if self.pending.is_empty() && self.candidates.is_empty() {
            return Some(QueryAction::QuerySucceeded {
                query: self.config.query,
            });
        }

        // check if any candidate has lower distance thant the current worst
        // `expect()` is ok because both `candidates` and `responses` have been confirmed to contain
        // entries
        if !self.candidates.is_empty() {
            let first_candidate_distance = self
                .config
                .target
                .distance(&self.candidates.first_key_value().expect("candidate to exist").1.key);
            let worst_response_candidate =
                *self.responses.last_entry().expect("response to exist").key();

            if first_candidate_distance < worst_response_candidate
                && self.pending.len() < self.config.parallelism_factor
            {
                return self.schedule_next_peer();
            }

            return Some(QueryAction::QuerySucceeded {
                query: self.config.query,
            });
        }

        if self.responses.len() == self.config.replication_factor {
            return Some(QueryAction::QuerySucceeded {
                query: self.config.query,
            });
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
