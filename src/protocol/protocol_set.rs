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
    codec::ProtocolCodec,
    crypto::ed25519::Keypair,
    error::Error,
    peer_id::PeerId,
    protocol::{
        connection::{ConnectionHandle, Permit},
        Direction, Transport, TransportEvent,
    },
    substream::Substream,
    transport::manager::{ProtocolContext, TransportManagerEvent, TransportManagerHandle},
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    DEFAULT_CHANNEL_SIZE,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    fmt::Debug,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

/// Logging target for the file.
const LOG_TARGET: &str = "protocol-set";

/// Maximum connections per peer.
const MAX_CONNECTIONS_PER_PEER: usize = 2;

pub enum InnerTransportEvent {
    /// Connection established to `peer`.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Connection ID.
        connection: ConnectionId,

        /// Address of remote peer.
        address: Multiaddr,

        /// Handle for communicating with the connection.
        sender: ConnectionHandle,
    },

    /// Connection closed.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,

        /// Connection ID.
        connection: ConnectionId,
    },

    /// Failed to dial peer.
    ///
    /// This is reported to that protocol which initiated the connection.
    DialFailure {
        /// Peer ID.
        peer: PeerId,

        /// Dialed address.
        address: Multiaddr,
    },

    /// Substream opened for `peer`.
    SubstreamOpened {
        /// Peer ID.
        peer: PeerId,

        /// Protocol name.
        ///
        /// One protocol handler may handle multiple sub-protocols (such as `/ipfs/identify/1.0.0`
        /// and `/ipfs/identify/push/1.0.0`) or it may have aliases which should be handled by
        /// the same protocol handler. When the substream is sent from transport to the protocol
        /// handler, the protocol name that was used to negotiate the substream is also sent so
        /// the protocol can handle the substream appropriately.
        protocol: ProtocolName,

        /// Substream direction.
        ///
        /// Informs the protocol whether the substream is inbound (opened by the remote node)
        /// or outbound (opened by the local node). This allows the protocol to distinguish
        /// between the two types of substreams and execute correct code for the substream.
        ///
        /// Outbound substreams also contain the substream ID which allows the protocol to
        /// distinguish between different outbound substreams.
        direction: Direction,

        /// Substream.
        substream: Box<dyn Substream>,
    },

    /// Failed to open substream.
    ///
    /// Substream open failures are reported only for outbound substreams.
    SubstreamOpenFailure {
        /// Substream ID.
        substream: SubstreamId,

        /// Error that occurred when the substream was being opened.
        error: Error,
    },
}

impl From<InnerTransportEvent> for TransportEvent {
    fn from(event: InnerTransportEvent) -> Self {
        match event {
            InnerTransportEvent::ConnectionEstablished { peer, address, .. } => {
                TransportEvent::ConnectionEstablished { peer, address }
            }
            InnerTransportEvent::ConnectionClosed { peer, .. } => {
                TransportEvent::ConnectionClosed { peer }
            }
            InnerTransportEvent::DialFailure { peer, address } => {
                TransportEvent::DialFailure { peer, address }
            }
            InnerTransportEvent::SubstreamOpened {
                peer,
                protocol,
                direction,
                substream,
            } => TransportEvent::SubstreamOpened {
                peer,
                protocol,
                direction,
                substream,
            },
            InnerTransportEvent::SubstreamOpenFailure { substream, error } => {
                TransportEvent::SubstreamOpenFailure { substream, error }
            }
        }
    }
}

#[derive(Debug)]
pub struct TransportService {
    /// Local peer ID.
    pub(crate) local_peer_id: PeerId,

    /// Protocol.
    protocol: ProtocolName,

    /// Open connections.
    // TODO: this can be refactored
    connections: HashMap<PeerId, Vec<(ConnectionHandle, ConnectionId)>>,

    /// Transport handle.
    transport_handle: TransportManagerHandle,

    /// RX channel for receiving events from tranports and connections.
    rx: Receiver<InnerTransportEvent>,

    /// Next substream ID.
    next_substream_id: Arc<AtomicUsize>,

    /// Pending keep-alive timeouts.
    keep_alive_timeouts: FuturesUnordered<BoxFuture<'static, (PeerId, ConnectionId)>>,
}

impl TransportService {
    /// Create new [`TransportService`].
    pub(crate) fn new(
        local_peer_id: PeerId,
        protocol: ProtocolName,
        next_substream_id: Arc<AtomicUsize>,
        transport_handle: TransportManagerHandle,
    ) -> (Self, Sender<InnerTransportEvent>) {
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                rx,
                protocol,
                local_peer_id,
                transport_handle,
                next_substream_id,
                connections: HashMap::new(),
                keep_alive_timeouts: FuturesUnordered::new(),
            },
            tx,
        )
    }

    /// Handle connection established event.
    fn on_connection_established(
        &mut self,
        peer: PeerId,
        address: Multiaddr,
        connection: ConnectionId,
        handle: ConnectionHandle,
    ) -> Option<TransportEvent> {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            ?address,
            ?connection,
            "connection established"
        );

        let event = match self.connections.entry(peer) {
            Entry::Occupied(entry) => {
                let entry = entry.into_mut();
                if entry.len() == MAX_CONNECTIONS_PER_PEER {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?peer,
                        ?address,
                        ?connection,
                        "connection established but already at maximum"
                    );
                    return None;
                }

                tracing::trace!(target: LOG_TARGET, ?peer, ?connection, "secondary connection");

                // user is not notified of the second connection
                entry.push((handle, connection));
                None
            }
            Entry::Vacant(entry) => {
                tracing::trace!(target: LOG_TARGET, ?peer, ?connection, "inform protocol");

                entry.insert(vec![(handle, connection)]);
                Some(TransportEvent::ConnectionEstablished { peer, address })
            }
        };

        self.keep_alive_timeouts.push(Box::pin(async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            (peer, connection)
        }));

        event
    }
}

#[async_trait::async_trait]
impl Transport for TransportService {
    async fn dial(&mut self, peer: &PeerId) -> crate::Result<()> {
        self.transport_handle.dial(peer).await
    }

    async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        self.transport_handle.dial_address(address).await
    }

    fn add_known_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>) {
        let addresses: HashSet<Multiaddr> = addresses
            .filter_map(|address| {
                if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
                    Some(address.with(Protocol::P2p(Multihash::from_bytes(&peer.to_bytes()).ok()?)))
                } else {
                    Some(address)
                }
            })
            .collect();

        self.transport_handle
            .add_know_address(peer, addresses.into_iter());
    }

    async fn open_substream(&mut self, peer: PeerId) -> crate::Result<SubstreamId> {
        // always prefer the first connection
        let connection = &mut self
            .connections
            .get_mut(&peer)
            .ok_or(Error::PeerDoesntExist(peer))?
            .get_mut(0)
            .ok_or(Error::PeerDoesntExist(peer))?
            .0;

        let permit = connection.try_get_permit().ok_or(Error::ConnectionClosed)?;
        let substream_id =
            SubstreamId::from(self.next_substream_id.fetch_add(1usize, Ordering::Relaxed));

        tracing::trace!(
            target: LOG_TARGET,
            protocol = ?self.protocol,
            ?peer,
            ?substream_id,
            "open substream",
        );

        connection
            .open_substream(self.protocol.clone(), substream_id, permit)
            .await
            .map(|_| substream_id)
    }

    async fn next_event(&mut self) -> Option<TransportEvent> {
        loop {
            tokio::select! {
                event = self.rx.recv() => match event? {
                    InnerTransportEvent::ConnectionEstablished {
                        peer,
                        address,
                        sender,
                        connection,
                    } => {
                        if let Some(event) = self.on_connection_established(peer, address, connection, sender) {
                            return Some(event)
                        }
                    }
                    InnerTransportEvent::ConnectionClosed { peer, connection } => match self.connections.get_mut(&peer) {
                        Some(connections) => {
                            connections.retain(|(_, id)| &connection != id);
                            return Some(TransportEvent::ConnectionClosed { peer })
                        }
                        None => {
                            tracing::warn!(target: LOG_TARGET, ?peer, ?connection, "closed connection doesn't exist");
                        }
                    }
                    event => return Some(event.into()),
                },
                peer = self.keep_alive_timeouts.next(), if !self.keep_alive_timeouts.is_empty() => {
                    match peer {
                        None => {
                            tracing::warn!(target: LOG_TARGET, "read `None` from `keep_alive_timeouts`");
                        }
                        Some((peer, connection)) => {
                            if let Some(connections) = self.connections.get_mut(&peer) {
                                tracing::debug!(target: LOG_TARGET, ?peer, "keep-alive timeout over, downgrade connection");

                                match connections.iter_mut().find(|(_, id)| id == &connection) {
                                    Some((connection, _)) => connection.close(),
                                    None => tracing::warn!(target: LOG_TARGET, ?peer, ?connection, "connection not found"),
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Events emitted by the installed protocols to transport.
#[derive(Debug)]
pub enum ProtocolCommand {
    /// Open substream.
    OpenSubstream {
        /// Protocol name.
        protocol: ProtocolName,

        /// Substream ID.
        ///
        /// Protocol allocates an ephemeral ID for outbound substreams which allows it to track
        /// the state of its pending substream. The ID is given back to protocol in
        /// [`TransportEvent::SubstreamOpened`]/[`TransportEvent::SubstreamOpenFailure`].
        ///
        /// This allows the protocol to distinguish inbound substreams from outbound substreams
        /// and associate incoming substreams with whatever logic it has.
        substream_id: SubstreamId,

        /// Connection permit.
        ///
        /// `Permit` allows the connection to be kept open while the permit is held and it is given
        /// to the substream to hold once it has been opened. When the substream is dropped, the permit
        /// is dropped and the connection may be closed if no other permit is being held.
        permit: Permit,
    },
}

/// Supported protocol information.
///
/// Each connection gets a copy of [`ProtocolSet`] which allows it to interact
/// directly with installed protocols.
#[derive(Debug)]
pub struct ProtocolSet {
    /// Installed protocols.
    pub(crate) protocols: HashMap<ProtocolName, ProtocolContext>,
    pub(crate) keypair: Keypair,
    mgr_tx: Sender<TransportManagerEvent>,
    connection: ConnectionHandle,
    rx: Receiver<ProtocolCommand>,
    next_substream_id: Arc<AtomicUsize>,
}

impl ProtocolSet {
    pub fn new(
        keypair: Keypair,
        mgr_tx: Sender<TransportManagerEvent>,
        next_substream_id: Arc<AtomicUsize>,
        protocols: HashMap<ProtocolName, ProtocolContext>,
    ) -> Self {
        let (tx, rx) = channel(256);

        ProtocolSet {
            rx,
            mgr_tx,
            keypair,
            protocols,
            next_substream_id,
            connection: ConnectionHandle::new(tx),
        }
    }

    /// Try to acquire permit to keep the connection open.
    pub fn try_get_permit(&mut self) -> Option<Permit> {
        self.connection.try_get_permit()
    }

    /// Get next substream ID.
    pub fn next_substream_id(&self) -> SubstreamId {
        SubstreamId::from(self.next_substream_id.fetch_add(1usize, Ordering::Relaxed))
    }

    /// Report to `protocol` that substream was opened for `peer`.
    pub async fn report_substream_open(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        direction: Direction,
        substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?peer, "substream opened");

        self.protocols
            .get_mut(&protocol)
            .ok_or(Error::ProtocolNotSupported(protocol.to_string()))?
            .tx
            .send(InnerTransportEvent::SubstreamOpened {
                peer,
                protocol: protocol.clone(),
                direction,
                substream,
            })
            .await
            .map_err(From::from)
    }

    /// Get codec used by the protocol.
    pub fn protocol_codec(&self, protocol: &ProtocolName) -> ProtocolCodec {
        // NOTE: `protocol` must exist in `self.protocol` as it was negotiated
        // using the protocols from this set
        self.protocols
            .get(&protocol)
            .expect("protocol to exist")
            .codec
    }

    /// Report to `protocol` that connection failed to open substream for `peer`.
    pub async fn report_substream_open_failure(
        &mut self,
        protocol: ProtocolName,
        substream: SubstreamId,
        error: Error,
    ) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            ?protocol,
            ?substream,
            ?error,
            "failed to open substream"
        );

        match self.protocols.get_mut(&protocol) {
            Some(info) => info
                .tx
                .send(InnerTransportEvent::SubstreamOpenFailure { substream, error })
                .await
                .map_err(From::from),
            None => Err(Error::ProtocolNotSupported(protocol.to_string())),
        }
    }

    // TODO: documentation
    pub(crate) async fn report_connection_established(
        &mut self,
        connection: ConnectionId,
        peer: PeerId,
        address: Multiaddr,
    ) -> crate::Result<()> {
        let connection_handle = self.connection.downgrade();

        for (_, sender) in &self.protocols {
            let _ = sender
                .tx
                .send(InnerTransportEvent::ConnectionEstablished {
                    peer,
                    connection,
                    address: address.clone(),
                    sender: connection_handle.clone(),
                })
                .await?;
        }

        self.mgr_tx
            .send(TransportManagerEvent::ConnectionEstablished {
                connection,
                peer,
                address,
            })
            .await
            .map_err(From::from)
    }

    /// Report to `Litep2p` that a peer disconnected.
    pub(crate) async fn report_connection_closed(
        &mut self,
        peer: PeerId,
        connection: ConnectionId,
    ) -> crate::Result<()> {
        for (_, sender) in &self.protocols {
            let _ = sender
                .tx
                .send(InnerTransportEvent::ConnectionClosed { peer, connection })
                .await?;
        }

        self.mgr_tx
            .send(TransportManagerEvent::ConnectionClosed { peer, connection })
            .await
            .map_err(From::from)
    }

    /// Poll next substream open query from one of the installed protocols.
    pub async fn next_event(&mut self) -> Option<ProtocolCommand> {
        self.rx.recv().await
    }
}
