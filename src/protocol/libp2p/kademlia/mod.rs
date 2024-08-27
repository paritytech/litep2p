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

//! [`/ipfs/kad/1.0.0`](https://github.com/libp2p/specs/blob/master/kad-dht/README.md) implementation.

use crate::{
    error::{Error, SubstreamError},
    protocol::{
        libp2p::kademlia::{
            bucket::KBucketEntry,
            executor::{QueryContext, QueryExecutor, QueryResult},
            handle::KademliaCommand,
            message::KademliaMessage,
            query::{QueryAction, QueryEngine},
            record::ProviderRecord,
            routing_table::RoutingTable,
            store::MemoryStore,
            types::{ConnectionType, KademliaPeer, Key},
        },
        Direction, TransportEvent, TransportService,
    },
    substream::Substream,
    types::SubstreamId,
    PeerId,
};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{Receiver, Sender};

use std::{
    collections::{hash_map::Entry, HashMap},
    time::{Duration, Instant},
};

pub use self::handle::RecordsType;
pub use config::{Config, ConfigBuilder};
pub use handle::{
    IncomingRecordValidationMode, KademliaEvent, KademliaHandle, Quorum, RoutingTableUpdateMode,
};
pub use query::QueryId;
pub use record::{Key as RecordKey, PeerRecord, Record};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia";

/// Parallelism factor, `α`.
const PARALLELISM_FACTOR: usize = 3;

mod bucket;
mod config;
mod executor;
mod handle;
mod message;
mod query;
mod record;
mod routing_table;
mod store;
mod types;

mod schema {
    pub(super) mod kademlia {
        include!(concat!(env!("OUT_DIR"), "/kademlia.rs"));
    }
}

/// Peer action.
#[derive(Debug)]
enum PeerAction {
    /// Send `FIND_NODE` message to peer.
    SendFindNode(QueryId),

    /// Send `PUT_VALUE` message to peer.
    SendPutValue(Bytes),
}

/// Peer context.
#[derive(Default)]
struct PeerContext {
    /// Pending action, if any.
    pending_actions: HashMap<SubstreamId, PeerAction>,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    pub fn new() -> Self {
        Self {
            pending_actions: HashMap::new(),
        }
    }

    /// Add pending action for peer.
    pub fn add_pending_action(&mut self, substream_id: SubstreamId, action: PeerAction) {
        self.pending_actions.insert(substream_id, action);
    }
}

/// Main Kademlia object.
pub(crate) struct Kademlia {
    /// Transport service.
    service: TransportService,

    /// Local Kademlia key.
    local_key: Key<PeerId>,

    /// Connected peers,
    peers: HashMap<PeerId, PeerContext>,

    /// TX channel for sending events to `KademliaHandle`.
    event_tx: Sender<KademliaEvent>,

    /// RX channel for receiving commands from `KademliaHandle`.
    cmd_rx: Receiver<KademliaCommand>,

    /// Routing table.
    routing_table: RoutingTable,

    /// Replication factor.
    replication_factor: usize,

    /// Record store.
    store: MemoryStore,

    /// Pending outbound substreams.
    pending_substreams: HashMap<SubstreamId, PeerId>,

    /// Pending dials.
    pending_dials: HashMap<PeerId, Vec<PeerAction>>,

    /// Routing table update mode.
    update_mode: RoutingTableUpdateMode,

    /// Incoming records validation mode.
    validation_mode: IncomingRecordValidationMode,

    /// Default record TTL.
    record_ttl: Duration,

    /// Provider record TTL.
    provider_ttl: Duration,

    /// Query engine.
    engine: QueryEngine,

    /// Query executor.
    executor: QueryExecutor,
}

impl Kademlia {
    /// Create new [`Kademlia`].
    pub(crate) fn new(mut service: TransportService, config: Config) -> Self {
        let local_peer_id = service.local_peer_id;
        let local_key = Key::from(service.local_peer_id);
        let mut routing_table = RoutingTable::new(local_key.clone());

        for (peer, addresses) in config.known_peers {
            tracing::trace!(target: LOG_TARGET, ?peer, ?addresses, "add bootstrap peer");

            routing_table.add_known_peer(peer, addresses.clone(), ConnectionType::NotConnected);
            service.add_known_address(&peer, addresses.into_iter());
        }

        Self {
            service,
            routing_table,
            peers: HashMap::new(),
            cmd_rx: config.cmd_rx,
            store: MemoryStore::new(),
            event_tx: config.event_tx,
            local_key,
            pending_dials: HashMap::new(),
            executor: QueryExecutor::new(),
            pending_substreams: HashMap::new(),
            update_mode: config.update_mode,
            validation_mode: config.validation_mode,
            record_ttl: config.record_ttl,
            provider_ttl: config.provider_ttl,
            replication_factor: config.replication_factor,
            engine: QueryEngine::new(local_peer_id, config.replication_factor, PARALLELISM_FACTOR),
        }
    }

    /// Connection established to remote peer.
    fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                if let KBucketEntry::Occupied(entry) = self.routing_table.entry(Key::from(peer)) {
                    entry.connection = ConnectionType::Connected;
                }

                let Some(actions) = self.pending_dials.remove(&peer) else {
                    entry.insert(PeerContext::new());
                    return Ok(());
                };

                // go over all pending actions, open substreams and save the state to `PeerContext`
                // from which it will be later queried when the substream opens
                let mut context = PeerContext::new();

                for action in actions {
                    match self.service.open_substream(peer) {
                        Ok(substream_id) => {
                            context.add_pending_action(substream_id, action);
                        }
                        Err(error) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?action,
                                ?error,
                                "connection established to peer but failed to open substream",
                            );

                            if let PeerAction::SendFindNode(query_id) = action {
                                self.engine.register_response_failure(query_id, peer);
                            }
                        }
                    }
                }

                entry.insert(context);
                Ok(())
            }
            Entry::Occupied(_) => Err(Error::PeerAlreadyExists(peer)),
        }
    }

    /// Disconnect peer from `Kademlia`.
    ///
    /// Peer is disconnected either because the substream was detected closed
    /// or because the connection was closed.
    ///
    /// The peer is kept in the routing table but its connection state is set
    /// as `NotConnected`, meaning it can be evicted from a k-bucket if another
    /// peer that shares the bucket connects.
    async fn disconnect_peer(&mut self, peer: PeerId, query: Option<QueryId>) {
        tracing::trace!(target: LOG_TARGET, ?peer, ?query, "disconnect peer");

        if let Some(query) = query {
            self.engine.register_response_failure(query, peer);
        }

        if let Some(PeerContext { pending_actions }) = self.peers.remove(&peer) {
            pending_actions.into_iter().for_each(|(_, action)| {
                if let PeerAction::SendFindNode(query_id) = action {
                    self.engine.register_response_failure(query_id, peer);
                }
            });
        }

        if let KBucketEntry::Occupied(entry) = self.routing_table.entry(Key::from(peer)) {
            entry.connection = ConnectionType::NotConnected;
        }
    }

    /// Local node opened a substream to remote node.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        substream: Substream,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?substream_id,
            "outbound substream opened",
        );
        let _ = self.pending_substreams.remove(&substream_id);

        let pending_action = &mut self
            .peers
            .get_mut(&peer)
            .ok_or(Error::PeerDoesntExist(peer))?
            .pending_actions
            .remove(&substream_id);

        match pending_action.take() {
            None => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?substream_id,
                    "pending action doesn't exist for peer, closing substream",
                );

                let _ = substream.close().await;
                return Ok(());
            }
            Some(PeerAction::SendFindNode(query)) => {
                match self.engine.next_peer_action(&query, &peer) {
                    Some(QueryAction::SendMessage {
                        query,
                        peer,
                        message,
                    }) => {
                        tracing::trace!(target: LOG_TARGET, ?peer, ?query, "start sending message to peer");

                        self.executor.send_request_read_response(
                            peer,
                            Some(query),
                            message,
                            substream,
                        );
                    }
                    // query finished while the substream was being opened
                    None => {
                        let _ = substream.close().await;
                    }
                    action => {
                        tracing::warn!(target: LOG_TARGET, ?query, ?peer, ?action, "unexpected action for `FIND_NODE`");
                        let _ = substream.close().await;
                        debug_assert!(false);
                    }
                }
            }
            Some(PeerAction::SendPutValue(message)) => {
                tracing::trace!(target: LOG_TARGET, ?peer, "send `PUT_VALUE` response");

                self.executor.send_message(peer, message, substream);
            }
        }

        Ok(())
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(&mut self, peer: PeerId, substream: Substream) {
        tracing::trace!(target: LOG_TARGET, ?peer, "inbound substream opened");

        self.executor.read_message(peer, None, substream);
    }

    /// Update routing table if the routing table update mode was set to automatic.
    ///
    /// Inform user about the potential routing table, allowing them to update it manually if
    /// the mode was set to manual.
    async fn update_routing_table(&mut self, peers: &[KademliaPeer]) {
        let peers: Vec<_> =
            peers.iter().filter(|peer| peer.peer != self.service.local_peer_id).collect();

        // inform user about the routing table update, regardless of what the routing table update
        // mode is
        let _ = self
            .event_tx
            .send(KademliaEvent::RoutingTableUpdate {
                peers: peers.iter().map(|peer| peer.peer).collect::<Vec<PeerId>>(),
            })
            .await;

        for info in peers {
            self.service.add_known_address(&info.peer, info.addresses.iter().cloned());

            if std::matches!(self.update_mode, RoutingTableUpdateMode::Automatic) {
                self.routing_table.add_known_peer(
                    info.peer,
                    info.addresses.clone(),
                    self.peers
                        .get(&info.peer)
                        .map_or(ConnectionType::NotConnected, |_| ConnectionType::Connected),
                );
            }
        }
    }

    /// Handle received message.
    async fn on_message_received(
        &mut self,
        peer: PeerId,
        query_id: Option<QueryId>,
        message: BytesMut,
        substream: Substream,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, query = ?query_id, "handle message from peer");

        match KademliaMessage::from_bytes(message).ok_or(Error::InvalidData)? {
            ref message @ KademliaMessage::FindNode {
                ref target,
                ref peers,
            } => {
                match query_id {
                    Some(query_id) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            ?target,
                            query = ?query_id,
                            "handle `FIND_NODE` response",
                        );

                        // update routing table and inform user about the update
                        self.update_routing_table(peers).await;
                        self.engine.register_response(query_id, peer, message.clone());
                    }
                    None => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            ?target,
                            "handle `FIND_NODE` request",
                        );

                        let message = KademliaMessage::find_node_response(
                            target,
                            self.routing_table
                                .closest(Key::from(target.clone()), self.replication_factor),
                        );
                        self.executor.send_message(peer, message.into(), substream);
                    }
                }
            }
            KademliaMessage::PutValue { record } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    record_key = ?record.key,
                    "handle `PUT_VALUE` message",
                );

                if let IncomingRecordValidationMode::Automatic = self.validation_mode {
                    self.store.put(record.clone());
                }

                let _ = self.event_tx.send(KademliaEvent::IncomingRecord { record }).await;
            }
            ref message @ KademliaMessage::GetRecord {
                ref key,
                ref record,
                ref peers,
            } => {
                match (query_id, key) {
                    (Some(query_id), _) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            query = ?query_id,
                            ?peers,
                            ?record,
                            "handle `GET_VALUE` response",
                        );

                        // update routing table and inform user about the update
                        self.update_routing_table(peers).await;
                        self.engine.register_response(query_id, peer, message.clone());
                    }
                    (None, Some(key)) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            ?key,
                            "handle `GET_VALUE` request",
                        );

                        let value = self.store.get(key).cloned();
                        let closest_peers = self
                            .routing_table
                            .closest(Key::from(key.to_vec()), self.replication_factor);

                        let message = KademliaMessage::get_value_response(
                            (*key).clone(),
                            closest_peers,
                            value,
                        );
                        self.executor.send_message(peer, message.into(), substream);
                    }
                    (None, None) => tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?message,
                        "unable to handle `GET_RECORD` request with empty key",
                    ),
                }
            }
            KademliaMessage::AddProvider { key, providers } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?key,
                    ?providers,
                    "handle `ADD_PROVIDER` message",
                );

                match (providers.len(), providers.first()) {
                    (1, Some(provider)) =>
                        if provider.peer == peer {
                            self.store.put_provider(ProviderRecord {
                                key,
                                provider: peer,
                                addresses: provider.addresses.clone(),
                                expires: Instant::now() + self.provider_ttl,
                            });
                        } else {
                            tracing::trace!(
                                target: LOG_TARGET,
                                publisher = ?peer,
                                provider = ?provider.peer,
                                "ignoring `ADD_PROVIDER` message with `publisher` != `provider`"
                            )
                        },
                    (n, _) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            publisher = ?peer,
                            ?n,
                            "ignoring `ADD_PROVIDER` message with `n` != 1 providers"
                        )
                    }
                }
            }
            ref message @ KademliaMessage::GetProviders {
                ref key,
                ref peers,
                ref providers,
            } => {
                match (query_id, key) {
                    (Some(query_id), key) => {
                        // Note: key is not required, but can be non-empty. We just ignore it here.
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            query = ?query_id,
                            ?key,
                            ?peers,
                            ?providers,
                            "handle `GET_PROVIDERS` response",
                        );

                        // update routing table and inform user about the update
                        self.update_routing_table(peers).await;

                        self.engine.register_response(query_id, peer, message.clone());
                    }
                    (None, Some(key)) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            ?key,
                            "handle `GET_PROVIDERS` request",
                        );

                        let providers = self.store.get_providers(key);
                        // TODO: if local peer is among the providers, update its `ProviderRecord`
                        //       to have up-to-date addresses.
                        //       Requires https://github.com/paritytech/litep2p/issues/211.

                        let closer_peers = self
                            .routing_table
                            .closest(Key::from(key.to_vec()), self.replication_factor);

                        let message = KademliaMessage::get_providers_response(
                            key.clone(),
                            providers,
                            &closer_peers,
                        );
                        self.executor.send_message(peer, message.into(), substream);
                    }
                    (None, None) => tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?message,
                        "unable to handle `GET_PROVIDERS` request with empty key",
                    ),
                }
            }
        }

        Ok(())
    }

    /// Failed to open substream to remote peer.
    async fn on_substream_open_failure(
        &mut self,
        substream_id: SubstreamId,
        error: SubstreamError,
    ) {
        tracing::trace!(
            target: LOG_TARGET,
            ?substream_id,
            ?error,
            "failed to open substream"
        );

        let Some(peer) = self.pending_substreams.remove(&substream_id) else {
            tracing::debug!(
                target: LOG_TARGET,
                ?substream_id,
                "outbound substream failed for non-existent peer"
            );
            return;
        };

        if let Some(context) = self.peers.get_mut(&peer) {
            let query = match context.pending_actions.remove(&substream_id) {
                Some(PeerAction::SendFindNode(query)) => Some(query),
                _ => None,
            };

            self.disconnect_peer(peer, query).await;
        }
    }

    /// Handle dial failure.
    fn on_dial_failure(&mut self, peer: PeerId, address: Multiaddr) {
        tracing::trace!(target: LOG_TARGET, ?peer, ?address, "failed to dial peer");

        let Some(actions) = self.pending_dials.remove(&peer) else {
            return;
        };

        for action in actions {
            if let PeerAction::SendFindNode(query_id) = action {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    query = ?query_id,
                    ?address,
                    "report failure for pending query",
                );

                self.engine.register_response_failure(query_id, peer);
            }
        }
    }

    /// Handle next query action.
    async fn on_query_action(&mut self, action: QueryAction) -> Result<(), (QueryId, PeerId)> {
        match action {
            QueryAction::SendMessage { query, peer, .. } => loop {
                match self.service.open_substream(peer) {
                    Err(_) => {
                        tracing::trace!(target: LOG_TARGET, ?query, ?peer, "dial peer");

                        match self.service.dial(&peer) {
                            Ok(_) => match self.pending_dials.entry(peer) {
                                Entry::Occupied(entry) => {
                                    entry.into_mut().push(PeerAction::SendFindNode(query));
                                }
                                Entry::Vacant(entry) => {
                                    entry.insert(vec![PeerAction::SendFindNode(query)]);
                                }
                            },
                            Err(Error::AlreadyConnected) => {
                                continue;
                            }
                            Err(error) => {
                                tracing::trace!(target: LOG_TARGET, ?query, ?peer, ?error, "failed to dial peer");
                                self.engine.register_response_failure(query, peer);
                            }
                        }

                        return Ok(());
                    }
                    Ok(substream_id) => {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?query,
                            ?peer,
                            ?substream_id,
                            "open outbound substream for peer"
                        );

                        self.pending_substreams.insert(substream_id, peer);
                        self.peers
                            .entry(peer)
                            .or_default()
                            .pending_actions
                            .insert(substream_id, PeerAction::SendFindNode(query));

                        return Ok(());
                    }
                }
            },
            QueryAction::FindNodeQuerySucceeded {
                target,
                peers,
                query,
            } => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?query,
                    peer = ?target,
                    num_peers = ?peers.len(),
                    "`FIND_NODE` succeeded",
                );

                let _ = self
                    .event_tx
                    .send(KademliaEvent::FindNodeSuccess {
                        target,
                        query_id: query,
                        peers: peers.into_iter().map(|info| (info.peer, info.addresses)).collect(),
                    })
                    .await;
                Ok(())
            }
            QueryAction::PutRecordToFoundNodes { record, peers } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    record_key = ?record.key,
                    num_peers = ?peers.len(),
                    "store record to found peers",
                );
                let key = record.key.clone();
                let message = KademliaMessage::put_value(record);

                for peer in peers {
                    match self.service.open_substream(peer.peer) {
                        Ok(substream_id) => {
                            self.pending_substreams.insert(substream_id, peer.peer);
                            self.peers
                                .entry(peer.peer)
                                .or_default()
                                .pending_actions
                                .insert(substream_id, PeerAction::SendPutValue(message.clone()));
                        }
                        Err(_) => match self.service.dial(&peer.peer) {
                            Ok(_) => match self.pending_dials.entry(peer.peer) {
                                Entry::Occupied(entry) => {
                                    entry
                                        .into_mut()
                                        .push(PeerAction::SendPutValue(message.clone()));
                                }
                                Entry::Vacant(entry) => {
                                    entry.insert(vec![PeerAction::SendPutValue(message.clone())]);
                                }
                            },
                            Err(error) => {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?key,
                                    ?error,
                                    "failed to dial peer",
                                );
                            }
                        },
                    }
                }

                Ok(())
            }
            QueryAction::GetRecordQueryDone { query_id, records } => {
                let _ = self
                    .event_tx
                    .send(KademliaEvent::GetRecordSuccess {
                        query_id,
                        records: RecordsType::Network(records),
                    })
                    .await;
                Ok(())
            }
            QueryAction::QueryFailed { query } => {
                tracing::debug!(target: LOG_TARGET, ?query, "query failed");

                let _ = self.event_tx.send(KademliaEvent::QueryFailed { query_id: query }).await;
                Ok(())
            }
            QueryAction::QuerySucceeded { .. } => unreachable!(),
        }
    }

    /// [`Kademlia`] event loop.
    pub async fn run(mut self) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, "starting kademlia event loop");

        loop {
            // poll `QueryEngine` for next actions.
            while let Some(action) = self.engine.next_action() {
                if let Err((query, peer)) = self.on_query_action(action).await {
                    self.disconnect_peer(peer, Some(query)).await;
                }
            }

            tokio::select! {
                event = self.service.next() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        if let Err(error) = self.on_connection_established(peer) {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to handle established connection");
                        }
                    }
                    Some(TransportEvent::ConnectionClosed { peer }) => {
                        self.disconnect_peer(peer, None).await;
                    }
                    Some(TransportEvent::SubstreamOpened { peer, direction, substream, .. }) => {
                        match direction {
                            Direction::Inbound => self.on_inbound_substream(peer, substream).await,
                            Direction::Outbound(substream_id) => {
                                if let Err(error) = self.on_outbound_substream(peer, substream_id, substream).await {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?substream_id,
                                        ?error,
                                        "failed to handle outbound substream",
                                    );
                                }
                            }
                        }
                    },
                    Some(TransportEvent::SubstreamOpenFailure { substream, error }) => {
                        self.on_substream_open_failure(substream, error).await;
                    }
                    Some(TransportEvent::DialFailure { peer, address }) => self.on_dial_failure(peer, address),
                    None => return Err(Error::EssentialTaskClosed),
                },
                context = self.executor.next() => {
                    let QueryContext { peer, query_id, result } = context.unwrap();

                    match result {
                        QueryResult::SendSuccess { substream } => {
                            tracing::trace!(target: LOG_TARGET, ?peer, query = ?query_id, "message sent to peer");
                            let _ = substream.close().await;
                        }
                        QueryResult::ReadSuccess { substream, message } => {
                            tracing::trace!(target: LOG_TARGET, ?peer, query = ?query_id, "message read from peer");

                            if let Err(error) = self.on_message_received(peer, query_id, message, substream).await {
                                tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to process message");
                            }
                        }
                        QueryResult::SubstreamClosed | QueryResult::Timeout => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                query = ?query_id,
                                ?result,
                                "failed to read message from substream",
                            );

                            self.disconnect_peer(peer, query_id).await;
                        }
                    }
                }
                command = self.cmd_rx.recv() => {
                    match command {
                        Some(KademliaCommand::FindNode { peer, query_id }) => {
                            tracing::debug!(target: LOG_TARGET, ?peer, query = ?query_id, "starting `FIND_NODE` query");

                            self.engine.start_find_node(
                                query_id,
                                peer,
                                self.routing_table.closest(Key::from(peer), self.replication_factor).into()
                            );
                        }
                        Some(KademliaCommand::PutRecord { mut record, query_id }) => {
                            tracing::debug!(target: LOG_TARGET, query = ?query_id, key = ?record.key, "store record to DHT");

                            // For `PUT_VALUE` requests originating locally we are always the publisher.
                            record.publisher = Some(self.local_key.clone().into_preimage());

                            // Make sure TTL is set.
                            record.expires = record.expires.or_else(|| Some(Instant::now() + self.record_ttl));

                            let key = Key::new(record.key.clone());

                            self.store.put(record.clone());

                            self.engine.start_put_record(
                                query_id,
                                record,
                                self.routing_table.closest(key, self.replication_factor).into(),
                            );
                        }
                        Some(KademliaCommand::PutRecordToPeers { mut record, query_id, peers, update_local_store }) => {
                            tracing::debug!(target: LOG_TARGET, query = ?query_id, key = ?record.key, "store record to DHT to specified peers");

                            // Make sure TTL is set.
                            record.expires = record.expires.or_else(|| Some(Instant::now() + self.record_ttl));

                            if update_local_store {
                                self.store.put(record.clone());
                            }

                            // Put the record to the specified peers.
                            let peers = peers.into_iter().filter_map(|peer| {
                                if peer == self.service.local_peer_id {
                                    return None;
                                }

                                match self.routing_table.entry(Key::from(peer)) {
                                    KBucketEntry::Occupied(entry) => Some(entry.clone()),
                                    KBucketEntry::Vacant(entry) if !entry.addresses.is_empty() => Some(entry.clone()),
                                    _ => None,
                                }
                            }).collect();

                            self.engine.start_put_record_to_peers(
                                query_id,
                                record,
                                peers,
                            );
                        }
                        Some(KademliaCommand::GetRecord { key, quorum, query_id }) => {
                            tracing::debug!(target: LOG_TARGET, ?key, "get record from DHT");

                            match (self.store.get(&key), quorum) {
                                (Some(record), Quorum::One) => {
                                    let _ = self
                                        .event_tx
                                        .send(KademliaEvent::GetRecordSuccess { query_id, records: RecordsType::LocalStore(record.clone()) })
                                        .await;
                                }
                                (record, _) => {
                                    self.engine.start_get_record(
                                        query_id,
                                        key.clone(),
                                        self.routing_table.closest(Key::new(key.clone()), self.replication_factor).into(),
                                        quorum,
                                        if record.is_some() { 1 } else { 0 },
                                    );
                                }
                            }

                        }
                        Some(KademliaCommand::AddKnownPeer { peer, addresses }) => {
                            tracing::trace!(
                                target: LOG_TARGET,
                                ?peer,
                                ?addresses,
                                "add known peer",
                            );

                            self.routing_table.add_known_peer(
                                peer,
                                addresses.clone(),
                                self.peers
                                    .get(&peer)
                                    .map_or(ConnectionType::NotConnected, |_| ConnectionType::Connected),
                            );
                            self.service.add_known_address(&peer, addresses.into_iter());

                        }
                        Some(KademliaCommand::StoreRecord { mut record }) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                key = ?record.key,
                                "store record in local store",
                            );

                            // Make sure TTL is set.
                            record.expires = record.expires.or_else(|| Some(Instant::now() + self.record_ttl));

                            self.store.put(record);
                        }
                        None => return Err(Error::EssentialTaskClosed),
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::{
        codec::ProtocolCodec,
        crypto::ed25519::Keypair,
        transport::{
            manager::{limits::ConnectionLimitsConfig, TransportManager},
            KEEP_ALIVE_TIMEOUT,
        },
        types::protocol::ProtocolName,
        BandwidthSink,
    };
    use tokio::sync::mpsc::channel;

    #[allow(unused)]
    struct Context {
        _cmd_tx: Sender<KademliaCommand>,
        event_rx: Receiver<KademliaEvent>,
    }

    fn make_kademlia() -> (Kademlia, Context, TransportManager) {
        let (manager, handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
            ConnectionLimitsConfig::default(),
        );

        let peer = PeerId::random();
        let (transport_service, _tx) = TransportService::new(
            peer,
            ProtocolName::from("/kad/1"),
            Vec::new(),
            Default::default(),
            handle,
            KEEP_ALIVE_TIMEOUT,
        );
        let (event_tx, event_rx) = channel(64);
        let (_cmd_tx, cmd_rx) = channel(64);

        let config = Config {
            protocol_names: vec![ProtocolName::from("/kad/1")],
            known_peers: HashMap::new(),
            codec: ProtocolCodec::UnsignedVarint(None),
            replication_factor: 20usize,
            update_mode: RoutingTableUpdateMode::Automatic,
            validation_mode: IncomingRecordValidationMode::Automatic,
            record_ttl: Duration::from_secs(36 * 60 * 60),
            provider_ttl: Duration::from_secs(48 * 60 * 60),
            event_tx,
            cmd_rx,
        };

        (
            Kademlia::new(transport_service, config),
            Context { _cmd_tx, event_rx },
            manager,
        )
    }

    #[tokio::test]
    async fn check_get_records_update() {
        let (mut kademlia, _context, _manager) = make_kademlia();

        let key = RecordKey::from(vec![1, 2, 3]);
        let records = vec![
            // 2 peers backing the same record.
            PeerRecord {
                peer: PeerId::random(),
                record: Record::new(key.clone(), vec![0x1]),
            },
            PeerRecord {
                peer: PeerId::random(),
                record: Record::new(key.clone(), vec![0x1]),
            },
            // only 1 peer backing the record.
            PeerRecord {
                peer: PeerId::random(),
                record: Record::new(key.clone(), vec![0x2]),
            },
        ];

        let query_id = QueryId(1);
        let action = QueryAction::GetRecordQueryDone { query_id, records };
        assert!(kademlia.on_query_action(action).await.is_ok());

        // Check the local storage should not get updated.
        assert!(kademlia.store.get(&key).is_none());
    }

    #[tokio::test]
    async fn check_get_records_update_with_expired_records() {
        let (mut kademlia, _context, _manager) = make_kademlia();

        let key = RecordKey::from(vec![1, 2, 3]);
        let expired = std::time::Instant::now() - std::time::Duration::from_secs(10);
        let records = vec![
            // 2 peers backing the same record, one record is expired.
            PeerRecord {
                peer: PeerId::random(),
                record: Record {
                    key: key.clone(),
                    value: vec![0x1],
                    publisher: None,
                    expires: Some(expired),
                },
            },
            PeerRecord {
                peer: PeerId::random(),
                record: Record::new(key.clone(), vec![0x1]),
            },
            // 2 peer backing the record.
            PeerRecord {
                peer: PeerId::random(),
                record: Record::new(key.clone(), vec![0x2]),
            },
            PeerRecord {
                peer: PeerId::random(),
                record: Record::new(key.clone(), vec![0x2]),
            },
        ];

        let query_id = QueryId(1);
        let action = QueryAction::GetRecordQueryDone { query_id, records };
        assert!(kademlia.on_query_action(action).await.is_ok());

        // Check the local storage should not get updated.
        assert!(kademlia.store.get(&key).is_none());
    }
}
