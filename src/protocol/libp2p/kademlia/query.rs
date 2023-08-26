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
    protocol::libp2p::kademlia::types::{Distance, KademliaPeer, Key},
};

use std::collections::{BTreeMap, HashMap, VecDeque};

/// Logging target for the file.
const LOG_TARGET: &str = "ipfs::kademlia::query";

#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub struct QueryId(usize);

enum Query {
    FindNode {
        /// Target key.
        target: Key<PeerId>,

        /// Target peer ID.
        peer: PeerId,

        /// Active candidates.
        ///
        /// These are peers who the [`QueryEngine`] has selected for the next oubound queries
        /// and are in the state of pending, waiting an answer to be heard.
        active: HashMap<PeerId, KademliaPeer>,

        /// Queried candidates.
        ///
        /// These are the peers for whom the query has already been sent
        /// and who have either returned their closest peers or failed to answer.
        queried: HashMap<PeerId, KademliaPeer>,

        /// Candidates.
        candidates: VecDeque<KademliaPeer>,

        /// Responses.
        responses: BTreeMap<Distance, KademliaPeer>,
    },
}

/// Actions emitted by the [`QueryEngine`].
#[derive(Debug)]
pub enum QueryAction {
    /// Send query to nodes.
    SendFindNode {
        /// Query target.
        peer: KademliaPeer,
    },

    /// Query succeeded.
    QuerySucceeded {
        /// Target peer.
        target: PeerId,

        /// Found peers.
        peers: Vec<KademliaPeer>,
    },

    /// Query failed
    QueryFailed {
        /// Query ID.
        query: QueryId,
    },
}

/// Lookup status.
#[derive(Debug, PartialEq, Eq)]
enum LookupStatus {
    /// Lookup failed.
    Failed,

    /// Lookup is paused because 3 parallel requests are in progress.
    Paused,

    /// Get next peer from candidates.
    NextPeer,

    /// Lookup has succeeded with one or more peers.
    Success,
}

/// Kademlia query engine.
pub struct QueryEngine {
    /// Next query ID.
    next_query_id: usize,

    /// Parallelism factor.
    _parallelism: usize,

    /// Active queries.
    queries: HashMap<QueryId, Query>,
}

impl QueryEngine {
    /// Create new [`QueryEngine`].
    pub fn new(_parallelism: usize) -> Self {
        Self {
            _parallelism,
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

    /// Report that a peer failed to respond to query.
    // TODO: documentation
    pub fn register_response_failure(&mut self, query_id: QueryId, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, query = ?query_id, ?peer, "register response failure");

        let Some(query) = self.queries.get_mut(&query_id) else {
            tracing::warn!(target: LOG_TARGET, ?query_id, ?peer, "query doesn't exist");
            debug_assert!(false);
            return;
        };

        match query {
            Query::FindNode {
                active, queried, ..
            } => {
                let Some(peer) = active.remove(&peer) else {
                    tracing::warn!(target: LOG_TARGET, ?query_id, ?peer, "query doesn't exist");
                    debug_assert!(false);
                    return;
                };

                queried.insert(peer.peer, peer);
            }
        }
    }

    // TODO: documentation
    pub(super) fn register_find_node_response(
        &mut self,
        query_id: QueryId,
        peer: PeerId,
        peers: Vec<KademliaPeer>,
    ) {
        tracing::trace!(target: LOG_TARGET, query = ?query_id, "register `FIND_NODE` response");

        match self.queries.get_mut(&query_id) {
            None => {
                tracing::warn!(target: LOG_TARGET, ?query_id, "query doesn't exist");
                debug_assert!(false);
                return;
            }
            Some(Query::FindNode {
                target,
                active,
                candidates,
                responses,
                ..
            }) => {
                let Some(peer) = active.remove(&peer) else {
                    tracing::warn!(target: LOG_TARGET, ?query_id, ?peer, "received response from peer but didn't expect it");
                    debug_assert!(false);
                    return;
                };

                // calculate distance for `peer` from target and insert it if
                //  a) the map doesn't have 20 responses
                //  b) it can replace some other peer that has a higher distance
                let distance = target.distance(&peer.key);

                // TODO: could this be written in another way?
                match responses.len() < 20 {
                    true => {
                        responses.insert(distance, peer);
                    }
                    false => {
                        let mut entry = responses.last_entry().expect("entry to exist");
                        if entry.key() > &distance {
                            entry.insert(peer);
                        }
                    }
                }

                // TODO: this is bad
                candidates.extend(peers.clone());
                candidates
                    .make_contiguous()
                    .sort_by(|a, b| target.distance(&a.key).cmp(&target.distance(&b.key)));
            }
        }
    }

    /// Start `FIND_NODE` query on the network and return the first
    // TODO: documentation
    pub fn start_find_node(&mut self, peer: PeerId, candidates: VecDeque<KademliaPeer>) -> QueryId {
        let query_id = self.next_query_id();

        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            ?query_id,
            "start `FIND_NODE` query"
        );

        self.queries.insert(
            query_id,
            Query::FindNode {
                peer,
                candidates,
                active: HashMap::new(),
                queried: HashMap::new(),
                target: Key::from(peer),
                responses: BTreeMap::new(),
            },
        );

        query_id
    }

    // TODO: this is a hack
    pub fn target_peer(&self, query: QueryId) -> Option<PeerId> {
        if let Some(Query::FindNode { peer, .. }) = self.queries.get(&query) {
            return Some(*peer);
        }

        None
    }

    /// Check if Kademlia `FIND_NODE` lookup is finished.
    // TODO: documentation
    fn lookup_status(
        target: &Key<PeerId>,
        active: &HashMap<PeerId, KademliaPeer>,
        candidates: &VecDeque<KademliaPeer>,
        responses: &mut BTreeMap<Distance, KademliaPeer>,
    ) -> LookupStatus {
        // the query failed
        if responses.is_empty() && active.is_empty() && candidates.is_empty() {
            return LookupStatus::Failed;
        }

        // there are still possible peers to query or peers who are being queried
        if responses.len() < 20 && (!active.is_empty() || !candidates.is_empty()) {
            if active.len() == 3 || candidates.is_empty() {
                return LookupStatus::Paused;
            }

            return LookupStatus::NextPeer;
        }

        // query succeeded with one or more results
        if active.is_empty() && candidates.is_empty() {
            return LookupStatus::Success;
        }

        // check if any candidate has lower distance thant the current worst
        if !candidates.is_empty() {
            let first_candidate_distance = target.distance(&candidates[0].key);
            let worst_response_candidate = responses.last_entry().unwrap().key().clone();

            if first_candidate_distance < worst_response_candidate {
                return LookupStatus::NextPeer;
            }

            // TODO: this is probably not correct
            return LookupStatus::Success;
        }

        tracing::error!(target: LOG_TARGET, "why here");

        todo!();
    }

    /// Poll next action from [`QueryEngine`].
    // TODO: this has iterate over all quries
    pub fn next_action(&mut self, query_id: QueryId) -> Option<QueryAction> {
        let mut query = self.queries.get_mut(&query_id)?;

        match &mut query {
            Query::FindNode {
                target,
                active,
                candidates,
                responses,
                ..
            } => match QueryEngine::lookup_status(&target, &active, &candidates, responses) {
                LookupStatus::Failed => {
                    tracing::trace!(target: LOG_TARGET, ?query_id, "lookup failed");

                    self.queries.remove(&query_id);
                    return Some(QueryAction::QueryFailed { query: query_id });
                }
                LookupStatus::Paused => None,
                LookupStatus::NextPeer => {
                    tracing::trace!(target: LOG_TARGET, ?query_id, "get next peer");

                    let candidate = candidates.pop_front().expect("entry to exist");
                    tracing::trace!(target: LOG_TARGET, ?candidate, "current candidate");
                    active.insert(candidate.peer, candidate.clone());

                    Some(QueryAction::SendFindNode { peer: candidate })
                }
                LookupStatus::Success => {
                    tracing::trace!(target: LOG_TARGET, ?query_id, "lookup succeeded");

                    let peers = responses.values().cloned().collect();
                    let target = target.clone().into_preimage();
                    self.queries.remove(&query_id);

                    Some(QueryAction::QuerySucceeded { target, peers })
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::libp2p::kademlia::types::ConnectionType;

    #[test]
    fn query_fails() {
        let mut engine = QueryEngine::new(3usize);
        let target_peer = PeerId::random();
        let _target_key = Key::from(target_peer);

        let query = engine.start_find_node(
            target_peer,
            vec![
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
            ]
            .into(),
        );

        for _ in 0..4 {
            if let Some(QueryAction::SendFindNode { peer }) = engine.next_action(query) {
                engine.register_response_failure(query, peer.peer);
            }
        }

        if let Some(QueryAction::QueryFailed { query: failed }) = engine.next_action(query) {
            assert_eq!(failed, query);
        }

        assert!(engine.next_action(query).is_none());
    }

    #[test]
    fn lookup_paused() {
        let mut engine = QueryEngine::new(3usize);
        let target_peer = PeerId::random();
        let _target_key = Key::from(target_peer);

        let query = engine.start_find_node(
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
            let _ = engine.next_action(query);
        }

        assert!(engine.next_action(query).is_none());
    }

    #[test]
    fn query_succeeds() {
        let mut engine = QueryEngine::new(3usize);
        let target_peer = PeerId::random();
        let _target_key = Key::from(target_peer);

        let _query = engine.start_find_node(
            target_peer,
            vec![
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
                KademliaPeer::new(PeerId::random(), vec![], ConnectionType::NotConnected),
            ]
            .into(),
        );
    }

    #[test]
    fn lookup_status_paused() {
        let target = Key::from(PeerId::random());
        let active = HashMap::from_iter((0..3).map(|_| {
            let peer = PeerId::random();
            (
                peer,
                KademliaPeer::new(peer, vec![], ConnectionType::NotConnected),
            )
        }));
        let candidates = vec![KademliaPeer::new(
            PeerId::random(),
            vec![],
            ConnectionType::NotConnected,
        )]
        .into();
        let mut responses = BTreeMap::new();
        let _engine = QueryEngine::new(3usize);

        assert_eq!(
            QueryEngine::lookup_status(&target, &active, &candidates, &mut responses),
            LookupStatus::Paused,
        );
    }

    #[test]
    fn lookup_status_failed() {
        let target = Key::from(PeerId::random());
        let active = HashMap::new();
        let candidates = VecDeque::new();
        let mut responses = BTreeMap::new();
        let _engine = QueryEngine::new(3usize);

        assert_eq!(
            QueryEngine::lookup_status(&target, &active, &candidates, &mut responses),
            LookupStatus::Failed
        );
    }
}
