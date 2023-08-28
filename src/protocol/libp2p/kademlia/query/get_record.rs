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
    peer_id::PeerId,
    protocol::libp2p::kademlia::{
        message::KademliaMessage,
        query::{QueryAction, QueryId},
        record::Key as RecordKey,
        types::{Distance, KademliaPeer, Key},
    },
};

use std::collections::{BTreeMap, HashMap, VecDeque};

/// Logging target for the file.
const LOG_TARGET: &str = "ipfs::kademlia::query::get_value";

#[derive(Debug)]
pub struct GetRecordContext {
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

impl GetRecordContext {
    /// Create new [`GetRecordContext`].
    pub fn new(
        query: QueryId,
        target: Key<RecordKey>,
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
        let Some(peer) =  self.pending.remove(&peer) else {
            tracing::warn!(target: LOG_TARGET, ?peer, "pending peer doesn't exist");
            debug_assert!(false);
            return;
        };

        todo!();
    }

    /// Register `GET_VALUE` response from `peer`.
    pub fn register_response(&mut self, peer: PeerId, message: KademliaMessage) {
        let Some(peer) = self.pending.remove(&peer) else {
            tracing::warn!(target: LOG_TARGET, ?peer, "received response from peer but didn't expect it");
            debug_assert!(false);
            return;
        };

        todo!();
    }

    /// Get next action for `peer`.
    pub fn next_peer_action(&mut self, peer: &PeerId) -> Option<QueryAction> {
        todo!();
    }

    /// Schedule next peer for outbound `GET_VALUE` query.
    pub fn schedule_next_peer(&mut self) -> QueryAction {
        todo!();
    }

    /// Get next action for a `FIND_NODE` query.
    // TODO: refactor this function
    pub fn next_action(&mut self) -> Option<QueryAction> {
        todo!();
    }
}
