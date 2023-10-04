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
            handle::KademliaCommand,
            message::KademliaMessage,
            query::{QueryAction, QueryEngine, QueryId},
            routing_table::RoutingTable,
            store::MemoryStore,
            types::{ConnectionType, Key},
        },
        Direction, Transport, TransportEvent, TransportService,
    },
    substream::{Substream, SubstreamSet},
    types::SubstreamId,
    PeerId,
};

use bytes::BytesMut;
use futures::StreamExt;
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{Receiver, Sender};

use std::collections::{hash_map::Entry, HashMap};

pub use {
    config::{Config, ConfigBuilder},
    handle::{KademliaEvent, KademliaHandle, Quorum},
    record::{Key as RecordKey, Record},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia";

/// Kademlia replication factor, `k`.
const _REPLICATION_FACTOR: usize = 20;

/// Parallelism factor, `α`.
const PARALLELISM_FACTOR: usize = 3;

mod bucket;
mod config;
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
    SendPutValue(Record),
}

/// Peer context.
struct PeerContext {
    /// Pending action, if any.
    pending_action: Option<PeerAction>,
}

impl PeerContext {
    pub fn new() -> Self {
        Self {
            pending_action: None,
        }
    }

    /// Create new [`PeerContext`] with pending [`PeerAction`].
    pub fn with_pending_action(action: PeerAction) -> Self {
        Self {
            pending_action: Some(action),
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

    /// Substream set.
    substreams: SubstreamSet<PeerId, Substream>,

    /// TX channel for sending events to `KademliaHandle`.
    event_tx: Sender<KademliaEvent>,

    /// RX channel for receiving commands from `KademliaHandle`.
    cmd_rx: Receiver<KademliaCommand>,

    /// Routing table.
    routing_table: RoutingTable,

    /// Record store.
    store: MemoryStore,

    /// Pending outbound substreams.
    pending_substreams: HashMap<SubstreamId, PeerId>,

    /// Pending dials.
    pending_dials: HashMap<PeerId, PeerAction>,

    /// Query engine.
    engine: QueryEngine,
}

impl Kademlia {
    /// Create new [`Kademlia`].
    pub(crate) fn new(service: TransportService, config: Config) -> Self {
        let local_key = Key::from(service.local_peer_id);
        let mut routing_table = RoutingTable::new(local_key.clone());

        for (peer, addresses) in config.known_peers {
            routing_table.add_known_peer(peer, addresses.clone(), ConnectionType::NotConnected);
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
            substreams: SubstreamSet::new(),
            pending_substreams: HashMap::new(),
            engine: QueryEngine::new(config.replication_factor, PARALLELISM_FACTOR),
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                // TODO: add peer to routing table
                // TODO: verify that peer limit is respected
                let context = match self.pending_dials.remove(&peer) {
                    Some(action) => {
                        let _ = self.service.open_substream(peer).await; // TODO: don't ignore error
                        PeerContext::with_pending_action(action)
                    }
                    None => PeerContext::new(),
                };

                entry.insert(context);
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
        tracing::debug!(target: LOG_TARGET, ?peer, ?query, "disconnect peer");

        if let Some(query) = query {
            self.engine.register_response_failure(query, peer);
        }

        if let Some(substream) = self.substreams.remove(&peer) {
            let _ = substream.close().await;
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
        mut substream: Substream,
    ) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            ?substream_id,
            "outbound substream opened"
        );
        let _ = self.pending_substreams.remove(&substream_id);

        let pending_action =
            &mut self.peers.get_mut(&peer).ok_or(Error::PeerDoesntExist(peer))?.pending_action;

        match std::mem::replace(pending_action, None) {
            None => {
                self.substreams.insert(peer, substream);
                return Ok(());
            }
            Some(PeerAction::SendFindNode(query)) => {
                if let Some(QueryAction::SendMessage {
                    query,
                    peer,
                    message,
                }) = self.engine.next_peer_action(&query, &peer)
                {
                    match substream.send_framed(message).await {
                        Err(_) => self.disconnect_peer(peer, Some(query)).await,
                        Ok(_) => {
                            *pending_action = Some(PeerAction::SendFindNode(query));
                        }
                    }
                }
            }
            Some(PeerAction::SendPutValue(record)) => {
                let message = KademliaMessage::put_value(record);

                if let Err(_) = substream.send_framed(message).await {
                    self.disconnect_peer(peer, None).await;
                }
            }
        }

        self.substreams.insert(peer, substream);
        Ok(())
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        _substream: Substream,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "inbound substream opened");

        Ok(())
    }

    /// Handle received message.
    async fn on_message_received(&mut self, peer: PeerId, message: BytesMut) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "handle message from peer");

        match KademliaMessage::from_bytes(message).ok_or(Error::InvalidData)? {
            KademliaMessage::FindNodeRequest { target } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?target,
                    "handle `FIND_NODE` request"
                );

                let substream =
                    self.substreams.get_mut(&peer).ok_or(Error::SubstreamDoesntExist)?;

                let message = KademliaMessage::find_node_response(
                    self.routing_table.closest(Key::from(target), 20),
                );

                if let Err(_error) = substream.send_framed(message.into()).await {
                    // TODO: check if peer has an active query in progress
                    self.disconnect_peer(peer, None).await;
                }
            }
            ref message @ KademliaMessage::FindNodeResponse { ref peers } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?peers,
                    "handle `FIND_NODE` response"
                );

                for info in peers {
                    self.service.add_known_address(&info.peer, info.addresses.iter().cloned())
                }

                match self
                    .peers
                    .get_mut(&peer)
                    .ok_or(Error::PeerDoesntExist(peer))?
                    .pending_action
                    .take()
                {
                    Some(PeerAction::SendFindNode(query)) => {
                        self.engine.register_response(query, peer, message.clone());
                    }
                    action => {
                        tracing::debug!(target: LOG_TARGET, ?peer, ?action, "received an unexpected `FIND_NODE` response");
                        self.disconnect_peer(peer, None).await;
                    }
                }
            }
            KademliaMessage::PutValue { record } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    record_key = ?record.key,
                    "handle `PUT_VALUE` response"
                );

                self.store.put(record);
            }
            ref message @ KademliaMessage::GetRecordResponse { ref peers, .. } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?peers,
                    "handle `FIND_NODE` response"
                );

                for info in peers {
                    self.service.add_known_address(&info.peer, info.addresses.iter().cloned())
                }

                match self
                    .peers
                    .get_mut(&peer)
                    .ok_or(Error::PeerDoesntExist(peer))?
                    .pending_action
                    .take()
                {
                    // TODO: this is so incorrect
                    Some(PeerAction::SendFindNode(query)) => {
                        self.engine.register_response(query, peer, message.clone());
                    }
                    action => {
                        tracing::debug!(target: LOG_TARGET, ?peer, ?action, "received an unexpected `FIND_NODE` response");
                        self.disconnect_peer(peer, None).await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Failed to open substream to remote peer.
    async fn on_substream_open_failure(&mut self, substream: SubstreamId, error: Error) {
        tracing::debug!(
            target: LOG_TARGET,
            ?substream,
            ?error,
            "failed to open substream"
        );

        let Some(peer) = self.pending_substreams.remove(&substream) else {
            tracing::debug!(target: LOG_TARGET, ?substream, "outbound substream failed for non-existent peer");
            return;
        };

        if let Some(context) = self.peers.get(&peer) {
            let query = match context.pending_action {
                Some(PeerAction::SendFindNode(query)) => Some(query),
                _ => None,
            };

            self.disconnect_peer(peer, query).await;
        }
    }

    /// Handle dial failure.
    fn on_dial_failure(&mut self, peer: PeerId, address: Multiaddr) {
        tracing::debug!(target: LOG_TARGET, ?peer, ?address, "failed to dial peer");

        if let Some(PeerAction::SendFindNode(query)) = self.pending_dials.remove(&peer) {
            self.engine.register_response_failure(query, peer);
        }
    }

    /// Handle next query action.
    async fn on_query_action(&mut self, action: QueryAction) -> Result<(), (QueryId, PeerId)> {
        match action {
            QueryAction::SendMessage {
                query,
                peer,
                message,
            } => match self.substreams.get_mut(&peer) {
                Some(substream) => substream
                    .send_framed(message)
                    .await
                    .map(|_| {
                        self.peers.insert(
                            peer,
                            PeerContext::with_pending_action(PeerAction::SendFindNode(query)),
                        );
                        ()
                    })
                    .map_err(|_| (query, peer)),
                None => match self.service.open_substream(peer).await {
                    Err(_) => {
                        tracing::debug!(target: LOG_TARGET, ?query, ?peer, "dial peer");

                        let _ = self.service.dial(&peer).await;
                        self.pending_dials.insert(peer, PeerAction::SendFindNode(query));
                        Ok(())
                    }
                    Ok(substream_id) => {
                        tracing::trace!(target: LOG_TARGET, ?query, ?peer, ?substream_id, "open outbound substream for peer");

                        self.peers.insert(
                            peer,
                            PeerContext::with_pending_action(PeerAction::SendFindNode(query)),
                        );
                        self.pending_substreams.insert(substream_id, peer);

                        Ok(())
                    }
                },
            },
            QueryAction::FindNodeQuerySucceeded { target, peers, .. } => {
                let _ = self
                    .event_tx
                    .send(KademliaEvent::FindNodeResult {
                        target,
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
                    "store record to found peers"
                );

                for peer in peers {
                    match self.substreams.get_mut(&peer.peer) {
                        Some(substream) => {
                            if let Err(error) = substream
                                .send_framed(KademliaMessage::put_value(record.clone()))
                                .await
                            {
                                tracing::debug!(target: LOG_TARGET, ?error, "failed to send message to peer");
                                self.disconnect_peer(peer.peer, None).await;
                            }
                        }
                        None => match self.service.open_substream(peer.peer).await {
                            Ok(substream_id) => {
                                self.peers.insert(
                                    peer.peer,
                                    PeerContext::with_pending_action(PeerAction::SendPutValue(
                                        record.clone(),
                                    )),
                                );
                                self.pending_substreams.insert(substream_id, peer.peer);
                            }
                            Err(_) => {
                                let _ = self.service.dial(&peer.peer).await;
                                self.pending_dials
                                    .insert(peer.peer, PeerAction::SendPutValue(record.clone()));
                            }
                        },
                    }
                }

                Ok(())
            }
            QueryAction::GetRecordQueryDone { record } => {
                self.store.put(record.clone());

                let _ = self.event_tx.send(KademliaEvent::GetRecordResult { record }).await;
                Ok(())
            }
            QueryAction::QuerySucceeded { .. } => unreachable!(),
            QueryAction::QueryFailed { query } => {
                tracing::error!(target: LOG_TARGET, ?query, "QUERY FAILED");

                Ok(())
            }
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
                            Direction::Inbound => {
                                if let Err(error) = self.on_inbound_substream(peer, substream).await {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?error,
                                        "failed to handle inbound substream",
                                    );
                                }
                            }
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
                command = self.cmd_rx.recv() => {
                    match command {
                        Some(KademliaCommand::FindNode { peer }) => {
                            tracing::debug!(target: LOG_TARGET, ?peer, "starting `FIND_NODE` query");
                            let _ = self.engine.start_find_node(
                                peer,
                                self.routing_table.closest(Key::from(peer), 20).into()
                            );
                        }
                        Some(KademliaCommand::PutRecord { record }) => {
                            tracing::debug!(target: LOG_TARGET, key = ?record.key, "store record to DHT");

                            self.store.put(record.clone());
                            let key = Key::new(record.key.clone());

                            let _ = self.engine.start_put_record(
                                record,
                                self.routing_table.closest(key, 20).into(),
                            );
                        }
                        Some(KademliaCommand::GetRecord { key, quorum }) => {
                            tracing::debug!(target: LOG_TARGET, ?key, "get record from DHT");

                            match (self.store.get(&key), quorum) {
                                (Some(record), Quorum::One) => {
                                    let _ = self
                                        .event_tx
                                        .send(KademliaEvent::GetRecordResult { record: record.clone() })
                                        .await;
                                }
                                (record, _) => {
                                    let _ = self.engine.start_get_record(
                                        key.clone(),
                                        self.routing_table.closest(Key::new(key.clone()), 20).into(),
                                        quorum,
                                        if record.is_some() { 1 } else { 0 },
                                    );
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
                event = self.substreams.next() => match event {
                    Some((peer, message)) => match message {
                        Ok(message) => {
                            if let Err(error) = self.on_message_received(peer, message).await {
                                tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to handle message");
                            } else {
                                tracing::debug!(target: LOG_TARGET, ?peer, "message handled sucecssfully");
                            }
                        }
                        Err(error) => return Err(error), // TODO: don't return error here
                    },
                    None => return Err(Error::EssentialTaskClosed),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
        let (manager, handle) = TransportManager::new(Keypair::generate(), BandwidthSink::new());

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
            protocol: ProtocolName::from("/kad/1"),
            codec: ProtocolCodec::UnsignedVarint(None),
            replication_factor: 20usize,
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
