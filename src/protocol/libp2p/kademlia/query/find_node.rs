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

    /// Cached Kademlia message to send.
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

        let kad_message = KademliaMessage::find_node(config.target.clone().into_preimage());

        Self {
            config,
            kad_message,

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
            // Update the furthest peer if this response is closer.
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
            message: self.kad_message.clone(),
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
            message: self.kad_message.clone(),
        })
    }

    /// Check if the query cannot make any progress.
    ///
    /// Returns true when there are no pending responses and no candidates to query.
    fn is_done(&self) -> bool {
        self.pending.is_empty() && self.candidates.is_empty()
    }

    /// Get next action for a `FIND_NODE` query.
    pub fn next_action(&mut self) -> Option<QueryAction> {
        // If we cannot make progress, return the final result.
        // A query failed when we are not able to identify one single peer.
        if self.is_done() {
            return if self.responses.is_empty() {
                Some(QueryAction::QueryFailed {
                    query: self.config.query,
                })
            } else {
                Some(QueryAction::QuerySucceeded {
                    query: self.config.query,
                })
            };
        }

        // At this point, we either have pending responses or candidates to query; and we need more
        // results. Ensure we do not exceed the parallelism factor.
        if self.pending.len() == self.config.parallelism_factor {
            return None;
        }

        // Schedule the next peer to fill up the responses.
        if self.responses.len() < self.config.replication_factor {
            return self.schedule_next_peer();
        }

        // We can finish the query here, but check if there is a better candidate for the query.
        match (
            self.candidates.first_key_value(),
            self.responses.last_key_value(),
        ) {
            (Some((_, candidate_peer)), Some((worst_response_distance, _))) => {
                let first_candidate_distance = self.config.target.distance(&candidate_peer.key);
                if first_candidate_distance < *worst_response_distance {
                    return self.schedule_next_peer();
                }
            }

            _ => (),
        }

        // We have found enough responses and there are no better candidates to query.
        Some(QueryAction::QuerySucceeded {
            query: self.config.query,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::libp2p::kademlia::types::ConnectionType;

    fn default_config() -> FindNodeConfig<Vec<u8>> {
        FindNodeConfig {
            local_peer_id: PeerId::random(),
            replication_factor: 20,
            parallelism_factor: 10,
            query: QueryId(0),
            target: Key::new(vec![1, 2, 3].into()),
        }
    }

    fn peer_to_kad(peer: PeerId) -> KademliaPeer {
        KademliaPeer {
            peer,
            key: Key::from(peer),
            addresses: vec![],
            connection: ConnectionType::Connected,
        }
    }

    #[test]
    fn completes_when_no_candidates() {
        let config = default_config();
        let mut context = FindNodeContext::new(config, VecDeque::new());
        assert!(context.is_done());
        let event = context.next_action().unwrap();
        assert_eq!(event, QueryAction::QueryFailed { query: QueryId(0) });
    }

    #[test]
    fn fulfill_parallelism() {
        let config = FindNodeConfig {
            parallelism_factor: 3,
            ..default_config()
        };

        let in_peers_set = (0..3).map(|_| PeerId::random()).collect::<HashSet<_>>();
        let in_peers = in_peers_set.iter().map(|peer| peer_to_kad(*peer)).collect();
        let mut context = FindNodeContext::new(config, in_peers);

        for num in 0..3 {
            let event = context.next_action().unwrap();
            match event {
                QueryAction::SendMessage { query, peer, .. } => {
                    assert_eq!(query, QueryId(0));
                    // Added as pending.
                    assert_eq!(context.pending.len(), num + 1);
                    assert!(context.pending.contains_key(&peer));

                    // Check the peer is the one provided.
                    assert!(in_peers_set.contains(&peer));
                }
                _ => panic!("Unexpected event"),
            }
        }

        // Fulfilled parallelism.
        assert!(context.next_action().is_none());
    }
}
