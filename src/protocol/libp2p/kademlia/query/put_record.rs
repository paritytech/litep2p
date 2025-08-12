// Copyright 2025 litep2p developers
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
    protocol::libp2p::kademlia::{handle::Quorum, query::QueryAction, QueryId},
    PeerId,
};

use std::{collections::HashSet, num::NonZeroUsize};

/// Logging target for this file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::query::put_record";

/// Context for tracking `PUT_VALUE` responses from peers.
#[derive(Debug)]
pub struct PutRecordToFoundNodesContext {
    /// Query ID.
    pub query: QueryId,

    /// Quorum that needs to be reached for the query to succeed.
    peers_to_succeed: NonZeroUsize,

    /// Peers we're waiting for responses from.
    pending_peers: HashSet<PeerId>,

    /// Successfully responded peers.
    successful_peers: HashSet<PeerId>,

    /// Failed peers.
    failed_peers: HashSet<PeerId>,
}

impl PutRecordToFoundNodesContext {
    /// Create new [`PutRecordToFoundNodesContext`].
    pub fn new(query: QueryId, peers: Vec<PeerId>, quorum: Quorum) -> Self {
        Self {
            query,
            peers_to_succeed: match quorum {
                Quorum::One => NonZeroUsize::new(1).expect("1 > 0; qed"),
                Quorum::N(n) => n,
                Quorum::All =>
                    NonZeroUsize::new(std::cmp::max(peers.len(), 1)).expect("1 > 0; qed"),
            },
            pending_peers: peers.into_iter().collect(),
            successful_peers: HashSet::new(),
            failed_peers: HashSet::new(),
        }
    }

    /// Register successful response from peer.
    pub fn register_response(&mut self, peer: PeerId) {
        if self.pending_peers.remove(&peer) {
            self.successful_peers.insert(peer);
        } else {
            tracing::debug!(
                target: LOG_TARGET,
                query = ?self.query,
                ?peer,
                "`PutRecordToFoundNodesContext::register_response`: pending peer does not exist",
            );
        }
    }

    /// Register failed response from peer.
    pub fn register_response_failure(&mut self, peer: PeerId) {
        if self.pending_peers.remove(&peer) {
            self.failed_peers.insert(peer);
        } else {
            tracing::debug!(
                target: LOG_TARGET,
                query = ?self.query,
                ?peer,
                "`PutRecordToFoundNodesContext::register_response_failure`: pending peer does not exist",
            );
        }
    }

    /// Check if all responses have been received.
    pub fn is_finished(&self) -> bool {
        self.pending_peers.is_empty()
    }

    /// Check if all requests were successful.
    pub fn is_succeded(&self) -> bool {
        self.successful_peers.len() >= self.peers_to_succeed.get()
    }

    /// Get next action if the context is finished.
    pub fn next_action(&self) -> Option<QueryAction> {
        if self.is_finished() {
            if self.is_succeded() {
                Some(QueryAction::QuerySucceeded { query: self.query })
            } else {
                Some(QueryAction::QueryFailed { query: self.query })
            }
        } else {
            None
        }
    }
}

