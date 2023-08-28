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
    peer_id::PeerId,
    protocol::libp2p::kademlia::{
        message::KademliaMessage,
        record::{Key as RecordKey, Record},
        types::{Distance, KademliaPeer, Key},
    },
};

use std::collections::{BTreeMap, HashMap, VecDeque};

// TODO: store record key instead of the actual record

/// Logging target for the file.
const LOG_TARGET: &str = "ipfs::kademlia::query";

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct QueryId(usize);

/// Context for `FIND_NODE` queries.
#[derive(Debug)]
struct FindNodeContext<T: Clone + Into<Vec<u8>>> {
    /// Query ID.
    query: QueryId,

    /// Target key.
    target: Key<T>,

    /// Peers from whom the `QueryEngine` is waiting to hear a response.
    pending: HashMap<PeerId, KademliaPeer>,

    /// Queried candidates.
    ///
    /// These are the peers for whom the query has already been sent
    /// and who have either returned their closest peers or failed to answer.
    queried: HashMap<PeerId, KademliaPeer>,

    /// Candidates.
    candidates: VecDeque<KademliaPeer>,

    /// Responses.
    responses: BTreeMap<Distance, KademliaPeer>,

    /// Replication factor.
    replication_factor: usize,

    /// Parallelism factor.
    parallelism_factor: usize,
}

impl<T: Clone + Into<Vec<u8>>> FindNodeContext<T> {
    /// Create new [`FindNodeContext`].
    fn new(
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
    fn register_response_failure(&mut self, peer: PeerId) {
        let Some(peer) =  self.pending.remove(&peer) else {
            tracing::warn!(target: LOG_TARGET, ?peer, "pending peer doesn't exist");
            debug_assert!(false);
            return;
        };

        self.queried.insert(peer.peer, peer);
    }

    /// Register `FIND_NODE` response from `peer`.
    fn register_response(&mut self, peer: PeerId, peers: Vec<KademliaPeer>) {
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
        // TODO: replication factor must not be hardcoded
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
    fn next_peer_action(&mut self, peer: &PeerId) -> Option<QueryAction> {
        self.pending
            .contains_key(peer)
            .then_some(QueryAction::SendMessage {
                query: self.query,
                peer: *peer,
                message: KademliaMessage::find_node(self.target.clone().into_preimage()),
            })
    }

    /// Schedule next peer for outbound `FIND_NODE` query.
    fn schedule_next_peer(&mut self) -> QueryAction {
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
    fn next_action(&mut self) -> Option<QueryAction> {
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

/// Query type.
#[derive(Debug)]
enum QueryType {
    /// `FIND_NODE` query.
    FindNode {
        /// Context for the `FIND_NODE` query
        context: FindNodeContext<PeerId>,
    },

    /// `PUT_VALUE` query.
    PutRecord {
        /// Record that needs to be stored.
        record: Record,

        /// Context for the `FIND_NODE` query
        context: FindNodeContext<RecordKey>,
    },

    /// `GET_VALUE` query.
    GetRecord {
        /// Context for the `FIND_NODE` query
        context: FindNodeContext<RecordKey>,
    },
}

/// Query action.
#[derive(Debug)]
pub enum QueryAction {
    /// Send message to peer.
    SendMessage {
        /// Query ID.
        query: QueryId,

        /// Peer.
        peer: PeerId,

        /// Message.
        message: Vec<u8>,
    },

    /// `FIND_NODE` query succeeded.
    FindNodeQuerySucceeded {
        /// ID of the query that succeeded.
        query: QueryId,

        /// Target peer.
        target: PeerId,

        /// Peers that were found.
        peers: Vec<KademliaPeer>,
    },

    /// Store the record to nodest closest to target key.
    // TODO: horrible name
    PutRecordToFoundNodes {
        /// Target peer.
        record: Record,

        /// Peers for whom the `PUT_VALUE` must be sent to.
        peers: Vec<KademliaPeer>,
    },

    // TODO: remove
    /// Query succeeded.
    QuerySucceeded {
        /// ID of the query that succeeded.
        query: QueryId,
    },

    /// Query failed.
    QueryFailed {
        /// ID of the query that failed.
        query: QueryId,
    },
}

/// Kademlia query engine.
pub struct QueryEngine {
    /// Next query ID.
    next_query_id: usize,

    /// Replication factor.
    replication_factor: usize,

    /// Parallelism factor.
    parallelism_factor: usize,

    /// Active queries.
    queries: HashMap<QueryId, QueryType>,
}

impl QueryEngine {
    /// Create new [`QueryEngine`].
    pub fn new(replication_factor: usize, parallelism_factor: usize) -> Self {
        Self {
            replication_factor,
            parallelism_factor,
            next_query_id: 0usize,
            queries: HashMap::new(),
        }
    }

    /// Allocate next query ID.
    fn next_query_id(&mut self) -> QueryId {
        let query_id = self.next_query_id;
        self.next_query_id += 1;

        QueryId(query_id)
    }

    /// Start `FIND_NODE` query.
    pub fn start_find_node(
        &mut self,
        target: PeerId,
        candidates: VecDeque<KademliaPeer>,
    ) -> crate::Result<QueryId> {
        let query_id = self.next_query_id();

        tracing::debug!(
            target: LOG_TARGET,
            ?query_id,
            ?target,
            num_peers = ?candidates.len(),
            "start `FIND_NODE` query"
        );

        self.queries.insert(
            query_id,
            QueryType::FindNode {
                context: FindNodeContext::new(
                    query_id,
                    Key::from(target),
                    candidates,
                    self.replication_factor,
                    self.parallelism_factor,
                ),
            },
        );

        Ok(query_id)
    }

    /// Start `PUT_VALUE` query.
    pub fn start_put_record(
        &mut self,
        record: Record,
        candidates: VecDeque<KademliaPeer>,
    ) -> crate::Result<QueryId> {
        let query_id = self.next_query_id();

        tracing::debug!(
            target: LOG_TARGET,
            ?query_id,
            target = ?record.key,
            num_peers = ?candidates.len(),
            "start `PUT_VALUE` query"
        );

        let target = Key::new(record.key.clone());

        self.queries.insert(
            query_id,
            QueryType::PutRecord {
                record,
                context: FindNodeContext::new(
                    query_id,
                    target,
                    candidates,
                    self.replication_factor,
                    self.parallelism_factor,
                ),
            },
        );

        Ok(query_id)
    }

    /// Start `GET_VALUE` query.
    pub fn _start_get_record(
        &mut self,
        target: RecordKey,
        candidates: VecDeque<KademliaPeer>,
    ) -> crate::Result<QueryId> {
        let query_id = self.next_query_id();

        tracing::debug!(
            target: LOG_TARGET,
            ?query_id,
            ?target,
            num_peers = ?candidates.len(),
            "start `GET_VALUE` query"
        );

        let target = Key::new(target);

        self.queries.insert(
            query_id,
            QueryType::GetRecord {
                context: FindNodeContext::new(
                    query_id,
                    target,
                    candidates,
                    self.replication_factor,
                    self.parallelism_factor,
                ),
            },
        );

        Ok(query_id)
    }

    /// Register response failure from a queried peer.
    pub fn register_response_failure(&mut self, query: QueryId, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?query, ?peer, "register response failure");

        match self.queries.get_mut(&query) {
            None => {
                tracing::warn!(target: LOG_TARGET, ?query, ?peer, "query doesn't exist");
                debug_assert!(false);
                return;
            }
            Some(QueryType::FindNode { context }) => {
                context.register_response_failure(peer);
            }
            Some(QueryType::PutRecord { context, .. }) | Some(QueryType::GetRecord { context }) => {
                context.register_response_failure(peer);
            }
        }
    }

    /// Register that `response` received from `peer`.
    pub fn register_response(&mut self, query: QueryId, peer: PeerId, message: KademliaMessage) {
        if !message.is_response() {
            tracing::warn!(target: LOG_TARGET, ?query, ?peer, "tried to register non-response");
            return;
        }

        tracing::trace!(target: LOG_TARGET, ?query, ?peer, "register response");

        match self.queries.get_mut(&query) {
            None => {
                tracing::warn!(target: LOG_TARGET, ?query, ?peer, "query doesn't exist");
                debug_assert!(false);
                return;
            }
            Some(QueryType::FindNode { context }) => match message {
                KademliaMessage::FindNodeResponse { peers } => {
                    context.register_response(peer, peers);
                }
                _ => unreachable!(),
            },
            Some(QueryType::PutRecord { context, .. }) => match message {
                KademliaMessage::FindNodeResponse { peers } => {
                    context.register_response(peer, peers);
                }
                _ => unreachable!(),
            },
            Some(QueryType::GetRecord { context }) => match message {
                KademliaMessage::FindNodeResponse { peers } => {
                    context.register_response(peer, peers);
                }
                _ => unreachable!(),
            },
        }
    }

    /// Get next action for `peer` from the [`QueryEngine`].
    pub fn next_peer_action(&mut self, query: &QueryId, peer: &PeerId) -> Option<QueryAction> {
        tracing::trace!(target: LOG_TARGET, ?query, ?peer, "get next peer action");

        match self.queries.get_mut(query) {
            None => {
                tracing::warn!(target: LOG_TARGET, ?query, ?peer, "pending query doesn't exist for peer");
                debug_assert!(false);
                return None;
            }
            Some(QueryType::FindNode { context }) => return context.next_peer_action(peer),
            Some(QueryType::PutRecord { context, .. }) | Some(QueryType::GetRecord { context }) => {
                return context.next_peer_action(peer)
            }
        }
    }

    /// Handle query success by returning the queried value(s)
    /// and removing the query from [`QueryEngine`].
    fn on_query_succeeded(&mut self, query: QueryId) -> QueryAction {
        match self.queries.remove(&query).expect("query to exist") {
            QueryType::FindNode { context } => QueryAction::FindNodeQuerySucceeded {
                query,
                target: context.target.into_preimage(),
                peers: context
                    .responses
                    .into_iter()
                    .map(|(_, peer)| peer)
                    .collect::<Vec<_>>(),
            },
            QueryType::PutRecord { record, context } => QueryAction::PutRecordToFoundNodes {
                record,
                peers: context
                    .responses
                    .into_iter()
                    .map(|(_, peer)| peer)
                    .collect::<Vec<_>>(),
            },
            _ => todo!(),
        }
    }

    /// Handle query failure by removing the query from [`QueryEngine`] and
    /// returning the appropriate [`QueryAction`] to user.
    fn on_query_failed(&mut self, query: QueryId) -> QueryAction {
        let _ = self.queries.remove(&query).expect("query to exist");

        QueryAction::QueryFailed { query }
    }

    /// Get next action from the [`QueryEngine`].
    pub fn next_action(&mut self) -> Option<QueryAction> {
        for (_, state) in self.queries.iter_mut() {
            let action = match state {
                QueryType::FindNode { context } => context.next_action(),
                QueryType::PutRecord { context, .. } => context.next_action(),
                QueryType::GetRecord { context } => context.next_action(),
            };

            match action {
                Some(QueryAction::QuerySucceeded { query }) => {
                    return Some(self.on_query_succeeded(query));
                }
                Some(QueryAction::QueryFailed { query }) => {
                    return Some(self.on_query_failed(query))
                }
                Some(_) => return action,
                _ => continue,
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use multihash::{Code, Multihash};

    use super::*;
    use crate::protocol::libp2p::kademlia::types::ConnectionType;

    // make fixed peer id
    fn make_peer_id(first: u8, second: u8) -> PeerId {
        let mut peer_id = vec![0u8; 32];
        peer_id[0] = first;
        peer_id[1] = second;

        PeerId::from_bytes(
            &Multihash::wrap(Code::Identity.into(), &peer_id)
                .expect("The digest size is never too large")
                .to_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn query_fails() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let mut engine = QueryEngine::new(20usize, 3usize);
        let target_peer = PeerId::random();
        let _target_key = Key::from(target_peer);

        let query = engine
            .start_find_node(
                target_peer,
                vec![
                    KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                    KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                    KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                    KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                ]
                .into(),
            )
            .unwrap();

        for _ in 0..4 {
            if let Some(QueryAction::SendMessage { query, peer, .. }) = engine.next_action() {
                engine.register_response_failure(query, peer);
            }
        }

        if let Some(QueryAction::QueryFailed { query: failed }) = engine.next_action() {
            assert_eq!(failed, query);
        }

        assert!(engine.next_action().is_none());
    }

    #[test]
    fn lookup_paused() {
        let mut engine = QueryEngine::new(20usize, 3usize);
        let target_peer = PeerId::random();
        let _target_key = Key::from(target_peer);

        let _ = engine.start_find_node(
            target_peer,
            vec![
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
            ]
            .into(),
        );

        for _ in 0..3 {
            let _ = engine.next_action();
        }

        assert!(engine.next_action().is_none());
    }

    #[test]
    fn find_node_query_succeeds() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let mut engine = QueryEngine::new(20usize, 3usize);
        let target_peer = make_peer_id(0, 0);
        let target_key = Key::from(target_peer);

        let distances = {
            let mut distances = std::collections::BTreeMap::new();

            for i in 1..64 {
                let peer = make_peer_id(i, 0);
                let key = Key::from(peer);

                distances.insert(target_key.distance(&key), peer);
            }

            distances
        };
        let mut iter = distances.iter();

        // start find node with one known peer
        let _query = engine.start_find_node(
            target_peer,
            vec![KademliaPeer::new(
                *iter.next().unwrap().1,
                vec![],
                ConnectionType::NotConnected,
            )]
            .into(),
        );

        let action = engine.next_action();
        assert!(engine.next_action().is_none());

        // the one known peer responds with 3 other peers it knows
        match action {
            Some(QueryAction::SendMessage { query, peer, .. }) => {
                engine.register_response(
                    query,
                    peer,
                    KademliaMessage::FindNodeResponse {
                        peers: vec![
                            KademliaPeer::new(
                                *iter.next().unwrap().1,
                                vec![],
                                ConnectionType::NotConnected,
                            ),
                            KademliaPeer::new(
                                *iter.next().unwrap().1,
                                vec![],
                                ConnectionType::NotConnected,
                            ),
                            KademliaPeer::new(
                                *iter.next().unwrap().1,
                                vec![],
                                ConnectionType::NotConnected,
                            ),
                        ],
                    },
                );
            }
            _ => panic!("invalid event received"),
        }

        // send empty response for the last three nodes
        for _ in 0..3 {
            match engine.next_action() {
                Some(QueryAction::SendMessage { query, peer, .. }) => {
                    println!("next send message to {peer:?}");
                    engine.register_response(
                        query,
                        peer,
                        KademliaMessage::FindNodeResponse { peers: vec![] },
                    );
                }
                _ => panic!("invalid event received"),
            }
        }

        match engine.next_action() {
            Some(QueryAction::FindNodeQuerySucceeded { peers, .. }) => {
                assert_eq!(peers.len(), 4);
            }
            _ => panic!("invalid event received"),
        }

        assert!(engine.next_action().is_none());
    }

    #[test]
    fn put_record_succeeds() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let mut engine = QueryEngine::new(20usize, 3usize);
        let record_key = RecordKey::new(&vec![1, 2, 3, 4]);
        let target_key = Key::new(record_key.clone());
        let original_record = Record::new(record_key, vec![1, 3, 3, 7, 1, 3, 3, 8]);

        let distances = {
            let mut distances = std::collections::BTreeMap::new();

            for i in 1..64 {
                let peer = make_peer_id(i, 0);
                let key = Key::from(peer);

                distances.insert(target_key.distance(&key), peer);
            }

            distances
        };
        let mut iter = distances.iter();

        // start find node with one known peer
        let _query = engine.start_put_record(
            original_record.clone(),
            vec![KademliaPeer::new(
                *iter.next().unwrap().1,
                vec![],
                ConnectionType::NotConnected,
            )]
            .into(),
        );

        let action = engine.next_action();
        assert!(engine.next_action().is_none());

        // the one known peer responds with 3 other peers it knows
        match action {
            Some(QueryAction::SendMessage { query, peer, .. }) => {
                engine.register_response(
                    query,
                    peer,
                    KademliaMessage::FindNodeResponse {
                        peers: vec![
                            KademliaPeer::new(
                                *iter.next().unwrap().1,
                                vec![],
                                ConnectionType::NotConnected,
                            ),
                            KademliaPeer::new(
                                *iter.next().unwrap().1,
                                vec![],
                                ConnectionType::NotConnected,
                            ),
                            KademliaPeer::new(
                                *iter.next().unwrap().1,
                                vec![],
                                ConnectionType::NotConnected,
                            ),
                        ],
                    },
                );
            }
            _ => panic!("invalid event received"),
        }

        // send empty response for the last three nodes
        for _ in 0..3 {
            match engine.next_action() {
                Some(QueryAction::SendMessage { query, peer, .. }) => {
                    println!("next send message to {peer:?}");
                    engine.register_response(
                        query,
                        peer,
                        KademliaMessage::FindNodeResponse { peers: vec![] },
                    );
                }
                _ => panic!("invalid event received"),
            }
        }

        match engine.next_action() {
            Some(QueryAction::PutRecordToFoundNodes { peers, record }) => {
                assert_eq!(peers.len(), 4);
                assert_eq!(record.key, original_record.key);
                assert_eq!(record.value, original_record.value);
            }
            _ => panic!("invalid event received"),
        }

        assert!(engine.next_action().is_none());
    }
}
