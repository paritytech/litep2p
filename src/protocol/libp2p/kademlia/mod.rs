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
    error::Error,
    protocol::{
        libp2p::kademlia::{
            bucket::KBucketEntry,
            executor::{QueryContext, QueryExecutor, QueryResult},
            handle::KademliaCommand,
            message::KademliaMessage,
            query::{QueryAction, QueryEngine},
            routing_table::RoutingTable,
            store::MemoryStore,
            types::{ConnectionType, KademliaPeer, Key},
        },
        Direction, Transport, TransportEvent, TransportService,
    },
    substream::Substream,
    types::SubstreamId,
    PeerId,
};

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{Receiver, Sender};

use std::collections::{hash_map::Entry, HashMap};

pub use {
    config::{Config, ConfigBuilder},
    handle::{KademliaEvent, KademliaHandle, Quorum, RoutingTableUpdateMode},
    query::QueryId,
    record::{Key as RecordKey, Record},
};

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
    pub fn new() -> Self {
        Self {
            pending_actions: HashMap::new(),
        }
    }

    /// Create new [`PeerContext`] with pending [`PeerAction`].
    pub fn with_pending_action(substream_id: SubstreamId, action: PeerAction) -> Self {
        Self {
            pending_actions: HashMap::from_iter([(substream_id, action)]),
        }
    }
}

/// Main Kademlia object.
pub(crate) struct Kademlia {
    /// Transport service.
    service: TransportService,

    /// Local Kademlia key.
    _local_key: Key<PeerId>,

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
    pending_dials: HashMap<PeerId, PeerAction>,

    /// Routing table update mode.
    update_mode: RoutingTableUpdateMode,

    /// Query engine.
    engine: QueryEngine,

    /// Query executor.
    executor: QueryExecutor,
}

impl Kademlia {
    /// Create new [`Kademlia`].
    pub(crate) fn new(mut service: TransportService, config: Config) -> Self {
        let local_key = Key::from(service.local_peer_id);
        let mut routing_table = RoutingTable::new(local_key.clone());

        for (peer, addresses) in config.known_peers {
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
            _local_key: local_key,
            pending_dials: HashMap::new(),
            executor: QueryExecutor::new(),
            pending_substreams: HashMap::new(),
            update_mode: config.update_mode,
            replication_factor: config.replication_factor,
            engine: QueryEngine::new(config.replication_factor, PARALLELISM_FACTOR),
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                // TODO: add peer to routing table
                // TODO: verify that peer limit is respected
                match self.pending_dials.remove(&peer) {
                    Some(action) => match self.service.open_substream(peer).await {
                        Ok(substream_id) => {
                            entry.insert(PeerContext::with_pending_action(substream_id, action));
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
                    },
                    None => {
                        entry.insert(PeerContext::new());
                    }
                }

                Ok(())
            }
            Entry::Occupied(_) => return Err(Error::PeerAlreadyExists(peer)),
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

        self.peers.remove(&peer);

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

        match std::mem::replace(pending_action, None) {
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
    async fn update_routing_table(&mut self, peers: &Vec<KademliaPeer>) {
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
        tracing::trace!(target: LOG_TARGET, ?peer, ?query_id, "handle message from peer");

        match KademliaMessage::from_bytes(message).ok_or(Error::InvalidData)? {
            KademliaMessage::FindNodeRequest { target } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?target,
                    "handle `FIND_NODE` request",
                );

                let message = KademliaMessage::find_node_response(
                    self.routing_table.closest(Key::from(target), self.replication_factor),
                );
                self.executor.send_message(peer, message.into(), substream);
            }
            ref message @ KademliaMessage::FindNodeResponse { ref peers } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?peers,
                    "handle `FIND_NODE` response",
                );

                match query_id {
                    Some(query_id) => {
                        // update routing table and inform user about the update
                        self.update_routing_table(peers).await;
                        self.engine.register_response(query_id, peer, message.clone());
                    }
                    None => tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        "unexpected `FIND_NODE` response",
                    ),
                }
            }
            KademliaMessage::PutValue { record } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    record_key = ?record.key,
                    "handle `PUT_VALUE` response",
                );

                self.store.put(record);
            }
            ref message @ KademliaMessage::GetRecordResponse { ref peers, .. } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?peers,
                    "handle `GET_RECORD` response",
                );

                match query_id {
                    Some(query_id) => {
                        // update routing table and inform user about the update
                        self.update_routing_table(peers).await;
                        self.engine.register_response(query_id, peer, message.clone());
                    }
                    None => tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        "unexpected `GET_RECORD` response",
                    ),
                }
            }
        }

        Ok(())
    }

    /// Failed to open substream to remote peer.
    async fn on_substream_open_failure(&mut self, substream_id: SubstreamId, error: Error) {
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
        if let Some(PeerAction::SendFindNode(query)) = self.pending_dials.remove(&peer) {
            tracing::trace!(target: LOG_TARGET, ?peer, ?address, "failed to dial peer");

            self.engine.register_response_failure(query, peer);
        }
    }

    /// Handle next query action.
    async fn on_query_action(&mut self, action: QueryAction) -> Result<(), (QueryId, PeerId)> {
        match action {
            QueryAction::SendMessage { query, peer, .. } =>
                match self.service.open_substream(peer).await {
                    Err(_) => {
                        tracing::trace!(target: LOG_TARGET, ?query, ?peer, "dial peer");

                        match self.service.dial(&peer).await {
                            Ok(_) => {
                                self.pending_dials.insert(peer, PeerAction::SendFindNode(query));
                            }
                            Err(error) => {
                                tracing::trace!(target: LOG_TARGET, ?query, ?peer, ?error, "failed to dial peer");
                                self.engine.register_response_failure(query, peer);
                            }
                        }

                        Ok(())
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

                        Ok(())
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
                let message = KademliaMessage::put_value(record);

                for peer in peers {
                    match self.service.open_substream(peer.peer).await {
                        Ok(substream_id) => {
                            self.pending_substreams.insert(substream_id, peer.peer);
                            self.peers
                                .entry(peer.peer)
                                .or_default()
                                .pending_actions
                                .insert(substream_id, PeerAction::SendPutValue(message.clone()));
                        }
                        Err(_) => {
                            let _ = self.service.dial(&peer.peer).await;
                            self.pending_dials
                                .insert(peer.peer, PeerAction::SendPutValue(message.clone()));
                        }
                    }
                }

                Ok(())
            }
            QueryAction::GetRecordQueryDone { query_id, record } => {
                self.store.put(record.clone());

                let _ =
                    self.event_tx.send(KademliaEvent::GetRecordSuccess { query_id, record }).await;
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
                event = self.service.next_event() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        if let Err(error) = self.on_connection_established(peer).await {
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
                            tracing::trace!(target: LOG_TARGET, ?peer, ?query_id, "message sent to peer");
                            let _ = substream.close().await;
                        }
                        QueryResult::ReadSuccess { substream, message } => {
                            tracing::trace!(target: LOG_TARGET, ?peer, ?query_id, "message read from peer");

                            if let Err(error) = self.on_message_received(peer, query_id, message, substream).await {
                                tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to process message");
                            }
                        }
                        QueryResult::SubstreamClosed | QueryResult::Timeout => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?query_id,
                                ?result,
                                "failed to read message from substream",
                            );

                            self.disconnect_peer(peer, query_id).await;
                        }
                    }
                }
                command = self.cmd_rx.recv() => {
                    match command {
                        Some(KademliaCommand::FindNode { peer, tx }) => {
                            tracing::debug!(target: LOG_TARGET, ?peer, "starting `FIND_NODE` query");

                            let query_id = self.engine.start_find_node(
                                peer,
                                self.routing_table.closest(Key::from(peer), self.replication_factor).into()
                            );
                            let _ = tx.send(query_id);
                        }
                        Some(KademliaCommand::PutRecord { record, tx }) => {
                            tracing::debug!(target: LOG_TARGET, key = ?record.key, "store record to DHT");

                            self.store.put(record.clone());
                            let key = Key::new(record.key.clone());

                            let query_id = self.engine.start_put_record(
                                record,
                                self.routing_table.closest(key, self.replication_factor).into(),
                            );
                            let _ = tx.send(query_id);
                        }
                        Some(KademliaCommand::GetRecord { key, quorum, tx }) => {
                            tracing::debug!(target: LOG_TARGET, ?key, "get record from DHT");

                            match (self.store.get(&key), quorum) {
                                (Some(record), Quorum::One) => {
                                    // since the record was found from the store, allocate a dummy query ID
                                    let query_id = self.engine.next_query_id();
                                    let _ = tx.send(query_id);

                                    let _ = self
                                        .event_tx
                                        .send(KademliaEvent::GetRecordSuccess { query_id, record: record.clone() })
                                        .await;
                                }
                                (record, _) => {
                                    let query_id = self.engine.start_get_record(
                                        key.clone(),
                                        self.routing_table.closest(Key::new(key.clone()), self.replication_factor).into(),
                                        quorum,
                                        if record.is_some() { 1 } else { 0 },
                                    );
                                    let _ = tx.send(query_id);
                                }
                            }

                        }
                        Some(KademliaCommand::AddKnownPeer { peer, addresses }) => {
                            self.routing_table.add_known_peer(
                                peer,
                                addresses.clone(),
                                self.peers
                                    .get(&peer)
                                    .map_or(ConnectionType::NotConnected, |_| ConnectionType::Connected),
                            );
                            self.service.add_known_address(&peer, addresses.into_iter());

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
        codec::ProtocolCodec, crypto::ed25519::Keypair, transport::manager::TransportManager,
        types::protocol::ProtocolName, BandwidthSink,
    };
    use tokio::sync::mpsc::channel;

    #[allow(unused)]
    struct Context {
        _cmd_tx: Sender<KademliaCommand>,
        event_rx: Receiver<KademliaEvent>,
    }

    fn _make_kademlia() -> (Kademlia, Context, TransportManager) {
        let (manager, handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());

        let peer = PeerId::random();
        let (transport_service, _tx) = TransportService::new(
            peer,
            ProtocolName::from("/kad/1"),
            Vec::new(),
            Default::default(),
            handle,
        );
        let (event_tx, event_rx) = channel(64);
        let (_cmd_tx, cmd_rx) = channel(64);

        let config = Config {
            protocol_names: vec![ProtocolName::from("/kad/1")],
            known_peers: HashMap::new(),
            codec: ProtocolCodec::UnsignedVarint(None),
            replication_factor: 20usize,
            update_mode: RoutingTableUpdateMode::Automatic,
            event_tx,
            cmd_rx,
        };

        (
            Kademlia::new(transport_service, config),
            Context { _cmd_tx, event_rx },
            manager,
        )
    }
}
