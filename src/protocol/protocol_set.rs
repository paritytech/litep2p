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
    protocol::{
        connection::{ConnectionHandle, Permit},
        Direction, Transport, TransportEvent,
    },
    substream::Substream,
    transport::{
        manager::{ProtocolContext, TransportManagerEvent, TransportManagerHandle},
        Endpoint,
    },
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId, DEFAULT_CHANNEL_SIZE,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::protocol-set";

/// Events emitted by the underlying transport protocols.
pub enum InnerTransportEvent {
    /// Connection established to `peer`.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Connection ID.
        connection: ConnectionId,

        /// Endpoint.
        endpoint: Endpoint,

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

        /// Fallback name.
        ///
        /// If the substream was negotiated using a fallback name of the main protocol,
        /// `fallback` is `Some`.
        fallback: Option<ProtocolName>,

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
        substream: Substream,
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
            InnerTransportEvent::ConnectionEstablished { peer, endpoint, .. } =>
                TransportEvent::ConnectionEstablished { peer, endpoint },
            InnerTransportEvent::ConnectionClosed { peer, .. } =>
                TransportEvent::ConnectionClosed { peer },
            InnerTransportEvent::DialFailure { peer, address } =>
                TransportEvent::DialFailure { peer, address },
            InnerTransportEvent::SubstreamOpened {
                peer,
                protocol,
                fallback,
                direction,
                substream,
            } => TransportEvent::SubstreamOpened {
                peer,
                protocol,
                fallback,
                direction,
                substream,
            },
            InnerTransportEvent::SubstreamOpenFailure { substream, error } =>
                TransportEvent::SubstreamOpenFailure { substream, error },
        }
    }
}

/// Connection context for the peer.
///
/// Each peer is allowed to have at most two connections open. The first open connection is the
/// primary connections which the local node uses to open substreams to remote. Secondary connection
/// may be open if local and remote opened connections at the same time.
///
/// Secondary connection may be promoted to a primary connection if the primary connections closes
/// while the secondary connections remains open.
#[derive(Debug)]
struct ConnectionContext {
    /// Primary connection.
    primary: ConnectionHandle,

    /// Secondary connection, if it exists.
    secondary: Option<ConnectionHandle>,
}

impl ConnectionContext {
    /// Create new [`ConnectionContext`].
    fn new(primary: ConnectionHandle) -> Self {
        Self {
            primary,
            secondary: None,
        }
    }

    /// Downgrade connection to non-active which means it will be closed
    /// if there are no substreams open over it.
    fn downgrade(&mut self, connection_id: &ConnectionId) {
        if self.primary.connection_id() == connection_id {
            self.primary.close();
            return;
        }

        if let Some(handle) = &mut self.secondary {
            if handle.connection_id() == connection_id {
                handle.close();
                return;
            }
        }

        tracing::debug!(
            target: LOG_TARGET,
            primary = ?self.primary.connection_id(),
            secondary = ?self.secondary.as_ref().map(|handle| handle.connection_id()),
            ?connection_id,
            "connection doesn't exist, cannot downgrade",
        );
    }
}

/// Provides an interfaces for [`Litep2p`](crate::Litep2p) protocols to interact
/// with the underlying transport protocols.
#[derive(Debug)]
pub struct TransportService {
    /// Local peer ID.
    pub(crate) local_peer_id: PeerId,

    /// Protocol.
    protocol: ProtocolName,

    /// Fallback names for the protocol.
    fallback_names: Vec<ProtocolName>,

    /// Open connections.
    connections: HashMap<PeerId, ConnectionContext>,

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
        fallback_names: Vec<ProtocolName>,
        next_substream_id: Arc<AtomicUsize>,
        transport_handle: TransportManagerHandle,
    ) -> (Self, Sender<InnerTransportEvent>) {
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                rx,
                protocol,
                local_peer_id,
                fallback_names,
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
        endpoint: Endpoint,
        connection_id: ConnectionId,
        handle: ConnectionHandle,
    ) -> Option<TransportEvent> {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            protocol = %self.protocol,
            ?endpoint,
            ?connection_id,
            "connection established",
        );

        match self.connections.get_mut(&peer) {
            Some(context) => match context.secondary {
                Some(_) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?connection_id,
                        ?endpoint,
                        "ignoring third connection",
                    );
                    None
                }
                None => {
                    self.keep_alive_timeouts.push(Box::pin(async move {
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        (peer, connection_id)
                    }));
                    context.secondary = Some(handle);

                    None
                }
            },
            None => {
                self.connections.insert(peer, ConnectionContext::new(handle));
                self.keep_alive_timeouts.push(Box::pin(async move {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    (peer, connection_id)
                }));

                Some(TransportEvent::ConnectionEstablished { peer, endpoint })
            }
        }
    }

    /// Handle connection closed event.
    fn on_connection_closed(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
    ) -> Option<TransportEvent> {
        let Some(context) = self.connections.get_mut(&peer) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?peer,
                ?connection_id,
                "connection closed to a non-existent peer",
            );

            debug_assert!(false);
            return None;
        };

        // if the primary connection was closed, check if there exist a secondary connection
        // and if it does, convert the secondary connection a primary connection
        if context.primary.connection_id() == &connection_id {
            tracing::trace!(target: LOG_TARGET, ?peer, ?connection_id, "primary connection closed");

            match context.secondary.take() {
                None => {
                    self.connections.remove(&peer);
                    return Some(TransportEvent::ConnectionClosed { peer });
                }
                Some(handle) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?connection_id,
                        "switch to secondary connection",
                    );

                    context.primary = handle;
                    return None;
                }
            }
        }

        match context.secondary.take() {
            Some(handle) if handle.connection_id() == &connection_id => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    "secondary connection closed",
                );

                return None;
            }
            connection_state => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?connection_state,
                    "connection closed but it doesn't exist",
                );

                return None;
            }
        }
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

        self.transport_handle.add_known_address(peer, addresses.into_iter());
    }

    async fn open_substream(&mut self, peer: PeerId) -> crate::Result<SubstreamId> {
        // always prefer the primary connection
        let connection =
            &mut self.connections.get_mut(&peer).ok_or(Error::PeerDoesntExist(peer))?.primary;

        let permit = connection.try_get_permit().ok_or(Error::ConnectionClosed)?;
        let substream_id =
            SubstreamId::from(self.next_substream_id.fetch_add(1usize, Ordering::Relaxed));

        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            protocol = %self.protocol,
            ?substream_id,
            "open substream",
        );

        connection
            .open_substream(
                self.protocol.clone(),
                self.fallback_names.clone(),
                substream_id,
                permit,
            )
            .await
            .map(|_| substream_id)
    }

    async fn next_event(&mut self) -> Option<TransportEvent> {
        loop {
            tokio::select! {
                event = self.rx.recv() => match event? {
                    InnerTransportEvent::ConnectionEstablished {
                        peer,
                        endpoint,
                        sender,
                        connection,
                    } => {
                        if let Some(event) = self.on_connection_established(peer, endpoint, connection, sender) {
                            return Some(event)
                        }
                    }
                    InnerTransportEvent::ConnectionClosed { peer, connection } => {
                        if let Some(event) = self.on_connection_closed(peer, connection) {
                            return Some(event)
                        }
                    }
                    event => return Some(event.into()),
                },
                peer = self.keep_alive_timeouts.next(), if !self.keep_alive_timeouts.is_empty() => {
                    match peer {
                        None => tracing::warn!(target: LOG_TARGET, "read `None` from `keep_alive_timeouts`"),
                        Some((peer, connection_id)) => {
                            if let Some(context) = self.connections.get_mut(&peer) {
                                tracing::trace!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?connection_id,
                                    "keep-alive timeout over, downgrade connection",
                                );

                                context.downgrade(&connection_id);
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

        /// Fallback names.
        ///
        /// If the protocol has changed its name but wishes to suppor the old name(s), it must
        /// provide the old protocol names in `fallback_names`. These are fed into
        /// `multistream-select` which them attempts to negotiate a protocol for the substream
        /// using one of the provided names and if the substream is negotiated successfully, will
        /// report back the actual protocol name that was negotiated, in case the protocol
        /// needs to deal with the old version of the protocol in different way compared to
        /// the new version.
        fallback_names: Vec<ProtocolName>,

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
        /// to the substream to hold once it has been opened. When the substream is dropped, the
        /// permit is dropped and the connection may be closed if no other permit is being
        /// held.
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
    fallback_names: HashMap<ProtocolName, ProtocolName>,
}

impl ProtocolSet {
    pub fn new(
        keypair: Keypair,
        connection_id: ConnectionId,
        mgr_tx: Sender<TransportManagerEvent>,
        next_substream_id: Arc<AtomicUsize>,
        protocols: HashMap<ProtocolName, ProtocolContext>,
    ) -> Self {
        let (tx, rx) = channel(256);

        let fallback_names = protocols
            .iter()
            .map(|(protocol, context)| {
                context
                    .fallback_names
                    .iter()
                    .map(|fallback| (fallback.clone(), protocol.clone()))
                    .collect::<HashMap<_, _>>()
            })
            .flatten()
            .collect();

        ProtocolSet {
            rx,
            mgr_tx,
            keypair,
            protocols,
            next_substream_id,
            fallback_names,
            connection: ConnectionHandle::new(connection_id, tx),
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

    /// Get the list of all supported protocols.
    pub fn protocols(&self) -> Vec<ProtocolName> {
        self.protocols
            .keys()
            .cloned()
            .chain(self.fallback_names.keys().cloned())
            .collect()
    }

    /// Report to `protocol` that substream was opened for `peer`.
    pub async fn report_substream_open(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        direction: Direction,
        substream: Substream,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, %protocol, ?peer, ?direction, "substream opened");

        let (protocol, fallback) = match self.fallback_names.get(&protocol) {
            Some(main_protocol) => (main_protocol.clone(), Some(protocol)),
            None => (protocol, None),
        };

        self.protocols
            .get_mut(&protocol)
            .ok_or(Error::ProtocolNotSupported(protocol.to_string()))?
            .tx
            .send(InnerTransportEvent::SubstreamOpened {
                peer,
                protocol: protocol.clone(),
                fallback,
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
            .get(self.fallback_names.get(&protocol).map_or(protocol, |protocol| protocol))
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

    /// Report to protocols that a connection was established.
    pub(crate) async fn report_connection_established(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        endpoint: Endpoint,
    ) -> crate::Result<()> {
        let connection_handle = self.connection.downgrade();
        let mut futures = self
            .protocols
            .iter()
            .map(|(_, sender)| {
                let endpoint = endpoint.clone();
                let connection_handle = connection_handle.clone();

                async move {
                    sender
                        .tx
                        .send(InnerTransportEvent::ConnectionEstablished {
                            peer,
                            connection: connection_id,
                            endpoint,
                            sender: connection_handle,
                        })
                        .await
                }
            })
            .collect::<FuturesUnordered<_>>();

        while !futures.is_empty() {
            if let Some(Err(error)) = futures.next().await {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?error,
                    "failed to report closed connection",
                );
            }
        }

        self.mgr_tx
            .send(TransportManagerEvent::ConnectionEstablished {
                connection: connection_id,
                peer,
                endpoint,
            })
            .await
            .map_err(From::from)
    }

    /// Report to protocols that a connection was established.
    pub(crate) async fn report_connection_established_new(
        &mut self,
        connection_id: ConnectionId,
        peer: PeerId,
        endpoint: Endpoint,
    ) -> crate::Result<()> {
        let connection_handle = self.connection.downgrade();
        let mut futures = self
            .protocols
            .iter()
            .map(|(_, sender)| {
                let endpoint = endpoint.clone();
                let connection_handle = connection_handle.clone();

                async move {
                    sender
                        .tx
                        .send(InnerTransportEvent::ConnectionEstablished {
                            peer,
                            connection: connection_id,
                            endpoint,
                            sender: connection_handle,
                        })
                        .await
                }
            })
            .collect::<FuturesUnordered<_>>();

        while !futures.is_empty() {
            if let Some(Err(error)) = futures.next().await {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?error,
                    "failed to report closed connection",
                );
            }
        }

        Ok(())
    }

    /// Report to protocols that a connection was closed.
    pub(crate) async fn report_connection_closed(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
    ) -> crate::Result<()> {
        let mut futures = self
            .protocols
            .iter()
            .map(|(_, sender)| async move {
                sender
                    .tx
                    .send(InnerTransportEvent::ConnectionClosed {
                        peer,
                        connection: connection_id,
                    })
                    .await
            })
            .collect::<FuturesUnordered<_>>();

        while !futures.is_empty() {
            if let Some(Err(error)) = futures.next().await {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?error,
                    "failed to report closed connection",
                );
            }
        }

        self.mgr_tx
            .send(TransportManagerEvent::ConnectionClosed {
                peer,
                connection: connection_id,
            })
            .await
            .map_err(From::from)
    }
}

impl Stream for ProtocolSet {
    type Item = ProtocolCommand;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;
    use parking_lot::RwLock;

    use super::*;
    use crate::{
        mock::substream::MockSubstream, transport::manager::handle::InnerTransportManagerCommand,
    };

    #[tokio::test]
    async fn fallback_is_provided() {
        let (tx, _rx) = channel(64);
        let (tx1, _rx1) = channel(64);

        let mut protocol_set = ProtocolSet::new(
            Keypair::generate(),
            ConnectionId::from(0usize),
            tx,
            Default::default(),
            HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx1,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: vec![
                        ProtocolName::from("/notif/1/fallback/1"),
                        ProtocolName::from("/notif/1/fallback/2"),
                    ],
                },
            )]),
        );

        let expected_protocols = HashSet::from([
            ProtocolName::from("/notif/1"),
            ProtocolName::from("/notif/1/fallback/1"),
            ProtocolName::from("/notif/1/fallback/2"),
        ]);

        for protocol in protocol_set.protocols().iter() {
            assert!(expected_protocols.contains(protocol));
        }

        protocol_set
            .report_substream_open(
                PeerId::random(),
                ProtocolName::from("/notif/1/fallback/2"),
                Direction::Inbound,
                Substream::new_mock(PeerId::random(), Box::new(MockSubstream::new())),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn main_protocol_reported_if_main_protocol_negotiated() {
        let (tx, _rx) = channel(64);
        let (tx1, mut rx1) = channel(64);

        let mut protocol_set = ProtocolSet::new(
            Keypair::generate(),
            ConnectionId::from(0usize),
            tx,
            Default::default(),
            HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx1,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: vec![
                        ProtocolName::from("/notif/1/fallback/1"),
                        ProtocolName::from("/notif/1/fallback/2"),
                    ],
                },
            )]),
        );

        protocol_set
            .report_substream_open(
                PeerId::random(),
                ProtocolName::from("/notif/1"),
                Direction::Inbound,
                Substream::new_mock(PeerId::random(), Box::new(MockSubstream::new())),
            )
            .await
            .unwrap();

        match rx1.recv().await.unwrap() {
            InnerTransportEvent::SubstreamOpened {
                protocol, fallback, ..
            } => {
                assert!(fallback.is_none());
                assert_eq!(protocol, ProtocolName::from("/notif/1"));
            }
            _ => panic!("invalid event received"),
        }
    }

    #[tokio::test]
    async fn fallback_is_reported_to_protocol() {
        let (tx, _rx) = channel(64);
        let (tx1, mut rx1) = channel(64);

        let mut protocol_set = ProtocolSet::new(
            Keypair::generate(),
            ConnectionId::from(0usize),
            tx,
            Default::default(),
            HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx1,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: vec![
                        ProtocolName::from("/notif/1/fallback/1"),
                        ProtocolName::from("/notif/1/fallback/2"),
                    ],
                },
            )]),
        );

        protocol_set
            .report_substream_open(
                PeerId::random(),
                ProtocolName::from("/notif/1/fallback/2"),
                Direction::Inbound,
                Substream::new_mock(PeerId::random(), Box::new(MockSubstream::new())),
            )
            .await
            .unwrap();

        match rx1.recv().await.unwrap() {
            InnerTransportEvent::SubstreamOpened {
                protocol, fallback, ..
            } => {
                assert_eq!(fallback, Some(ProtocolName::from("/notif/1/fallback/2")));
                assert_eq!(protocol, ProtocolName::from("/notif/1"));
            }
            _ => panic!("invalid event received"),
        }
    }

    /// Create new `TransportService`
    fn transport_service() -> (
        TransportService,
        Sender<InnerTransportEvent>,
        Receiver<InnerTransportManagerCommand>,
    ) {
        let (cmd_tx, cmd_rx) = channel(64);
        let handle = TransportManagerHandle::new(
            Arc::new(RwLock::new(HashMap::new())),
            cmd_tx,
            HashSet::new(),
        );

        let (service, sender) = TransportService::new(
            PeerId::random(),
            ProtocolName::from("/notif/1"),
            Vec::new(),
            Arc::new(AtomicUsize::new(0usize)),
            handle,
        );

        (service, sender, cmd_rx)
    }

    #[tokio::test]
    async fn secondary_connection_stored() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next_event().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, _cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.next_event().poll_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );
    }

    #[tokio::test]
    async fn tertiary_connection_ignored() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next_event().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, _cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.next_event().poll_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );

        // try to register tertiary connection and verify it's ignored
        let (cmd_tx3, mut cmd_rx3) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(2usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(2usize)),
                sender: ConnectionHandle::new(ConnectionId::from(2usize), cmd_tx3),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.next_event().poll_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );
        assert!(cmd_rx3.try_recv().is_err());
    }

    #[tokio::test]
    async fn secondary_closing_doesnt_emit_event() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next_event().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, _cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.next_event().poll_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );

        // close the secondary connection
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(1usize),
            })
            .await
            .unwrap();

        // verify that the protocol is not notified
        futures::future::poll_fn(|cx| match service.next_event().poll_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        // verify that the secondary connection doesn't exist anymore
        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert!(context.secondary.is_none());
    }

    #[tokio::test]
    async fn convert_secondary_to_primary() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, mut cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next_event().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, mut cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.next_event().poll_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );

        // close the primary connection
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(0usize),
            })
            .await
            .unwrap();

        // verify that the protocol is not notified
        futures::future::poll_fn(|cx| match service.next_event().poll_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        // verify that the primary connection has been replaced
        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(1usize));
        assert!(context.secondary.is_none());
        assert!(cmd_rx1.try_recv().is_err());

        // close the secondary connection as well
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(1usize),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionClosed {
            peer: disconnected_peer,
        }) = service.next_event().await
        {
            assert_eq!(disconnected_peer, peer);
        } else {
            panic!("expected event from `TransportService`");
        };

        // verify that the primary connection has been replaced
        assert!(service.connections.get(&peer).is_none());
        assert!(cmd_rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn keep_alive_timeout_expires_for_a_stale_connection() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1337usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1337usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1337usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next_event().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // verify the first connection state is correct
        assert_eq!(service.keep_alive_timeouts.len(), 1);
        match service.connections.get(&peer) {
            Some(context) => {
                assert_eq!(
                    context.primary.connection_id(),
                    &ConnectionId::from(1337usize)
                );
                assert!(context.secondary.is_none());
            }
            None => panic!("expected {peer} to exist"),
        }

        // close the primary connection
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(1337usize),
            })
            .await
            .unwrap();

        // verify that the protocols are notified of the connection closing as well
        if let Some(TransportEvent::ConnectionClosed {
            peer: connected_peer,
        }) = service.next_event().await
        {
            assert_eq!(connected_peer, peer);
        } else {
            panic!("expected event from `TransportService`");
        }

        // verify that the keep-alive timeout still exists for the peer but the peer itself
        // doesn't exist anymore
        //
        // the peer is removed because there is no connection to them
        assert_eq!(service.keep_alive_timeouts.len(), 1);
        assert!(service.connections.get(&peer).is_none());

        // register new primary connection but verify that there are now two pending keep-alive
        // timeouts
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1338usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(1338usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1338usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next_event().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // verify the first connection state is correct
        assert_eq!(service.keep_alive_timeouts.len(), 2);
        match service.connections.get(&peer) {
            Some(context) => {
                assert_eq!(
                    context.primary.connection_id(),
                    &ConnectionId::from(1338usize)
                );
                assert!(context.secondary.is_none());
            }
            None => panic!("expected {peer} to exist"),
        }

        match tokio::time::timeout(Duration::from_secs(10), service.next_event()).await {
            Ok(event) => panic!("didn't expect an event: {event:?}"),
            Err(_) => {}
        }
    }
}
