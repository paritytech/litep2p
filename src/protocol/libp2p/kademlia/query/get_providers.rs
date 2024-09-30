// Copyright 2024 litep2p developers
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
        record::{ContentProvider, Key as RecordKey},
        types::{Distance, KademliaPeer, Key},
    },
    types::multiaddr::Multiaddr,
    PeerId,
};

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::query::get_providers";

/// The configuration needed to instantiate a new [`GetProvidersContext`].
#[derive(Debug)]
pub struct GetProvidersConfig {
    /// Local peer ID.
    pub local_peer_id: PeerId,

    /// Parallelism factor.
    pub parallelism_factor: usize,

    /// Query ID.
    pub query: QueryId,

    /// Target key.
    pub target: Key<RecordKey>,

    /// Known providers from the local store.
    pub known_providers: Vec<KademliaPeer>,
}

#[derive(Debug)]
pub struct GetProvidersContext {
    /// Query immutable config.
    pub config: GetProvidersConfig,

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

    /// Found providers.
    pub found_providers: Vec<KademliaPeer>,
}

impl GetProvidersContext {
    /// Create new [`GetProvidersContext`].
    pub fn new(config: GetProvidersConfig, in_peers: VecDeque<KademliaPeer>) -> Self {
        let mut candidates = BTreeMap::new();

        for candidate in &in_peers {
            let distance = config.target.distance(&candidate.key);
            candidates.insert(distance, candidate.clone());
        }

        let kad_message =
            KademliaMessage::get_providers_request(config.target.clone().into_preimage());

        Self {
            config,
            kad_message,
            candidates,
            pending: HashMap::new(),
            queried: HashSet::new(),
            found_providers: Vec::new(),
        }
    }

    /// Get the found providers.
    pub fn found_providers(self) -> Vec<ContentProvider> {
        Self::merge_and_sort_providers(
            self.config.known_providers.into_iter().chain(self.found_providers),
            self.config.target,
        )
    }

    fn merge_and_sort_providers(
        found_providers: impl IntoIterator<Item = KademliaPeer>,
        target: Key<RecordKey>,
    ) -> Vec<ContentProvider> {
        // Merge addresses of different provider records of the same peer.
        let mut providers = HashMap::<PeerId, HashSet<Multiaddr>>::new();
        found_providers.into_iter().for_each(|provider| {
            providers.entry(provider.peer).or_default().extend(provider.addresses)
        });

        // Convert into `Vec<KademliaPeer>`
        let mut providers = providers
            .into_iter()
            .map(|(peer, addresses)| ContentProvider {
                peer,
                addresses: addresses.into_iter().collect(),
            })
            .collect::<Vec<_>>();

        // Sort by the provider distance to the target key.
        providers.sort_unstable_by(|p1, p2| {
            Key::from(p1.peer).distance(&target).cmp(&Key::from(p2.peer).distance(&target))
        });

        providers
    }

    /// Register response failure for `peer`.
    pub fn register_response_failure(&mut self, peer: PeerId) {
        let Some(peer) = self.pending.remove(&peer) else {
            tracing::debug!(
                target: LOG_TARGET,
                query = ?self.config.query,
                ?peer,
                "`GetProvidersContext`: pending peer doesn't exist",
            );
            return;
        };

        self.queried.insert(peer.peer);
    }

    /// Register `GET_PROVIDERS` response from `peer`.
    pub fn register_response(
        &mut self,
        peer: PeerId,
        providers: impl IntoIterator<Item = KademliaPeer>,
        closer_peers: impl IntoIterator<Item = KademliaPeer>,
    ) {
        tracing::trace!(
            target: LOG_TARGET,
            query = ?self.config.query,
            ?peer,
            "`GetProvidersContext`: received response from peer",
        );

        let Some(peer) = self.pending.remove(&peer) else {
            tracing::debug!(
                target: LOG_TARGET,
                query = ?self.config.query,
                ?peer,
                "`GetProvidersContext`: received response from peer but didn't expect it",
            );
            return;
        };

        self.found_providers.extend(providers);

        // Add the queried peer to `queried` and all new peers which haven't been
        // queried to `candidates`
        self.queried.insert(peer.peer);

        let to_query_candidate = closer_peers.into_iter().filter_map(|peer| {
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
        tracing::trace!(
            target: LOG_TARGET,
            query = ?self.config.query,
            "`GetProvidersContext`: get next peer",
        );

        let (_, candidate) = self.candidates.pop_first()?;
        let peer = candidate.peer;

        tracing::trace!(
            target: LOG_TARGET,
            query = ?self.config.query,
            ?peer,
            "`GetProvidersContext`: current candidate",
        );
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

    /// Get next action for a `GET_PROVIDERS` query.
    pub fn next_action(&mut self) -> Option<QueryAction> {
        if self.is_done() {
            // If we cannot make progress, return the final result.
            // A query failed when we are not able to find any providers.
            if self.found_providers.is_empty() {
                Some(QueryAction::QueryFailed {
                    query: self.config.query,
                })
            } else {
                Some(QueryAction::QuerySucceeded {
                    query: self.config.query,
                })
            }
        } else if self.pending.len() == self.config.parallelism_factor {
            // At this point, we either have pending responses or candidates to query; and we need
            // more records. Ensure we do not exceed the parallelism factor.
            None
        } else {
            self.schedule_next_peer()
        }
    }
}
