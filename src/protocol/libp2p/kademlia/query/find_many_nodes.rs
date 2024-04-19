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
        query::{QueryAction, QueryId},
        types::KademliaPeer,
    },
    PeerId,
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::query::find_many_nodes";

/// Context for multiple `FIND_NODE` queries.
#[derive(Debug)]
pub struct FindManyNodesContext {
    /// Local peer ID.
    local_peer_id: PeerId,

    /// Query ID.
    pub query: QueryId,

    /// The peers we are looking for.
    pub peers_to_report: Vec<PeerId>,
}

impl FindManyNodesContext {
    /// Creates a new [`FindManyNodesContext`].
    pub fn new(local_peer_id: PeerId, query: QueryId, peers_to_report: Vec<PeerId>) -> Self {
        Self {
            local_peer_id,
            query,
            peers_to_report,
        }
    }

    /// Register response failure for `peer`.
    pub fn register_response_failure(&mut self, _peer: PeerId) {}

    /// Register `FIND_NODE` response from `peer`.
    pub fn register_response(&mut self, _peer: PeerId, _peers: Vec<KademliaPeer>) {}

    /// Get next action for `peer`.
    pub fn next_peer_action(&mut self, _peer: &PeerId) -> Option<QueryAction> {
        None
    }

    /// Get next action for a `FIND_NODE` query.
    pub fn next_action(&mut self) -> Option<QueryAction> {
        return Some(QueryAction::QuerySucceeded { query: self.query });
    }
}
