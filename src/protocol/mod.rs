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

//! Protocol-related defines.

use crate::{
    codec::{identity::Identity, unsigned_varint::UnsignedVarint, ProtocolCodec},
    crypto::ed25519::Keypair,
    error::Error,
    peer_id::PeerId,
    substream::{RawSubstream, Substream, SubstreamType},
    transport::manager::{TransportManagerEvent, TransportManagerHandle},
    types::{protocol::ProtocolName, SubstreamId},
    DEFAULT_CHANNEL_SIZE,
};

use multiaddr::Multiaddr;
use tokio::sync::mpsc::{channel, Receiver, Sender, WeakSender};
use tokio_util::codec::Framed;

use std::{collections::HashMap, fmt::Debug};

pub mod libp2p;
pub mod mdns;
pub mod notification;
pub mod request_response;

/// Logging target for the file.
const LOG_TARGET: &str = "protocol";

/// Substream direction.
#[derive(Debug, Copy, Clone)]
pub enum Direction {
    /// Substream was opened by the remote peer.
    Inbound,

    /// Substream was opened by the local peer.
    Outbound(SubstreamId),
}

/// Events emitted by one of the installed transports to protocol(s).
pub enum InnerTransportEvent {
    /// Connection established to `peer`.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Address of remote peer.
        address: Multiaddr,

        /// Handle for communicating with the connection.
        sender: Sender<ProtocolCommand>,
    },

    /// Connection closed.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
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

/// Events emitted by one of the installed transports to protocol(s).
pub enum TransportEvent {
    /// Connection established to `peer`.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Address of remote peer.
        address: Multiaddr,
    },

    /// Connection closed.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
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
            InnerTransportEvent::ConnectionClosed { peer } => {
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

#[async_trait::async_trait]
pub trait Transport {
    /// Dial `peer` using `PeerId`.
    ///
    /// Call fails if `Litep2p` doesn't know have a known address for the peer.
    async fn dial(&self, peer: &PeerId) -> crate::Result<()>;

    /// Dial peer using a `Multiaddr`.
    ///
    /// Call fails if the address is not in correct format or it contains an unsupported/disabled transport.
    ///
    /// Calling this function is only necessary for those addresses that are discovered out-of-band
    /// since `Litep2p` internally keeps track of all peer addresses it has learned through user
    /// calling this function, Kademlia peer discoveries and `Identify` responses.
    async fn dial_address(&self, address: Multiaddr) -> crate::Result<()>;

    /// Add known one or more addresses for peer.
    ///
    /// The list is filtered for duplicates and unsupported transports.
    fn add_known_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>);

    /// Disconnect peer from the protocol.
    ///
    /// This doesn't automatically close the connection as other protocols may have
    /// substream open on the connection but once the protocols have all closed their
    /// substreams on the connecction, it will be closed.
    fn disconnect(&mut self, peer: &PeerId) -> crate::Result<()>;

    /// Open substream to `peer`.
    ///
    /// Call fails if there is no open connection to the peer.
    async fn open_substream(&mut self, peer: PeerId) -> crate::Result<SubstreamId>;

    /// Get next event from the [`NewTransportService`].
    async fn next_event(&mut self) -> Option<TransportEvent>;
}

/// Connection type, from the point of view of the protocol.
#[derive(Debug)]
enum ConnectionType {
    /// Protocol wishes to keep the connection open.
    Active(Sender<ProtocolCommand>),

    /// Protocol is not interested in the connection and the connection will be closed
    /// due to keep-alive timeout if all protocols consider the connection inactive.
    _Inactive(WeakSender<ProtocolCommand>),
}

#[derive(Debug)]
pub struct TransportService {
    /// Local peer ID.
    local_peer_id: PeerId,

    /// Protocol.
    protocol: ProtocolName,

    /// Open connections.
    connections: HashMap<PeerId, ConnectionType>,

    /// Transport handle.
    transport_handle: TransportManagerHandle,

    /// RX channel for receiving events from tranports and connections.
    rx: Receiver<InnerTransportEvent>,

    /// Next substream ID.
    next_substream_id: SubstreamId,
}

impl TransportService {
    /// Create new [`TransportService`].
    pub(crate) fn new(
        local_peer_id: PeerId,
        protocol: ProtocolName,
        transport_handle: TransportManagerHandle,
    ) -> (Self, Sender<InnerTransportEvent>) {
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                rx,
                protocol,
                local_peer_id,
                transport_handle,
                connections: HashMap::new(),
                next_substream_id: SubstreamId::from(0usize),
            },
            tx,
        )
    }
}

#[async_trait::async_trait]
impl Transport for TransportService {
    async fn dial(&self, peer: &PeerId) -> crate::Result<()> {
        self.transport_handle.dial(peer).await
    }

    async fn dial_address(&self, address: Multiaddr) -> crate::Result<()> {
        self.transport_handle.dial_address(address).await
    }

    fn add_known_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>) {
        self.transport_handle.add_know_address(peer, addresses);
    }

    fn disconnect(&mut self, _peer: &PeerId) -> crate::Result<()> {
        todo!();
    }

    async fn open_substream(&mut self, peer: PeerId) -> crate::Result<SubstreamId> {
        let connection = self
            .connections
            .get_mut(&peer)
            .ok_or(Error::PeerDoesntExist(peer))?;
        let substream_id = self.next_substream_id.next();

        tracing::trace!(
            target: LOG_TARGET,
            protocol = ?self.protocol,
            ?peer,
            ?substream_id,
            "open substream",
        );

        match connection {
            ConnectionType::_Inactive(_) => todo!(),
            ConnectionType::Active(tx) => tx
                .send(ProtocolCommand::OpenSubstream {
                    protocol: self.protocol.clone(),
                    substream_id,
                })
                .await
                .map(|_| substream_id)
                .map_err(From::from),
        }
    }

    async fn next_event(&mut self) -> Option<TransportEvent> {
        match self.rx.recv().await? {
            InnerTransportEvent::ConnectionEstablished {
                peer,
                address,
                sender,
            } => {
                self.connections
                    .insert(peer, ConnectionType::Active(sender));
                Some(TransportEvent::ConnectionEstablished { peer, address })
            }
            InnerTransportEvent::ConnectionClosed { peer } => {
                self.connections.remove(&peer);
                Some(TransportEvent::ConnectionClosed { peer })
            }
            event => Some(event.into()),
        }
    }
}

/// Events emitted by the installed protocols to transport.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    },
}

#[async_trait::async_trait]
pub trait UserProtocol: Send + Debug {
    /// Get user protocol name.
    fn protocol(&self) -> ProtocolName;

    /// Get user protocol codec.
    fn codec(&self) -> ProtocolCodec;

    /// Start the the user protocol event loop.
    async fn run(self: Box<Self>, service: TransportService) -> crate::Result<()>;
}

/// Supported protocol information.
///
/// Each connection gets a copy of [`ProtocolSet`] which allows it to interact
/// directly with installed protocols.
#[derive(Debug)]
pub struct ProtocolSet {
    /// Installed protocols.
    pub(crate) protocols: HashMap<ProtocolName, crate::transport::manager::ProtocolContext>,
    pub(crate) keypair: Keypair,
    pub(crate) mgr_tx: Sender<TransportManagerEvent>,
    pub(crate) tx: Sender<ProtocolCommand>,
    pub(crate) rx: Receiver<ProtocolCommand>,
}

impl ProtocolSet {
    /// Report to `protocol` that substream was opened for `peer`.
    pub async fn report_substream_open<R: RawSubstream>(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        direction: Direction,
        substream: SubstreamType<R>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?peer, "substream opened");

        // TODO: transport should create the substream as it knows the internal representation
        // of the underlying I/O stream
        match self.protocols.get_mut(&protocol) {
            Some(info) => {
                let substream: Box<dyn Substream> = match substream {
                    SubstreamType::Raw(substream) => match info.codec {
                        ProtocolCodec::Identity(payload_size) => {
                            Box::new(Framed::new(substream, Identity::new(payload_size)))
                        }
                        ProtocolCodec::UnsignedVarint => {
                            Box::new(Framed::new(substream, UnsignedVarint::new()))
                        }
                    },
                    SubstreamType::ChannelBackend(mut substream) => {
                        substream.apply_codec(info.codec.clone());
                        Box::new(substream)
                    }
                    SubstreamType::Ready(substream) => substream,
                };

                info.tx
                    .send(InnerTransportEvent::SubstreamOpened {
                        peer,
                        protocol: protocol.clone(),
                        direction,
                        substream,
                    })
                    .await
                    .map_err(From::from)
            }
            None => Err(Error::ProtocolNotSupported(protocol.to_string())),
        }
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
        peer: PeerId,
        address: Multiaddr,
    ) -> crate::Result<()> {
        for (_, sender) in &self.protocols {
            let _ = sender
                .tx
                .send(InnerTransportEvent::ConnectionEstablished {
                    peer,
                    address: address.clone(),
                    sender: self.tx.clone(),
                })
                .await?;
        }

        self.mgr_tx
            .send(TransportManagerEvent::ConnectionEstablished { peer, address })
            .await
            .map_err(From::from)
    }

    /// Report to `Litep2p` that a peer disconnected.
    pub(crate) async fn report_connection_closed(&mut self, peer: PeerId) -> crate::Result<()> {
        for (_, sender) in &self.protocols {
            let _ = sender
                .tx
                .send(InnerTransportEvent::ConnectionClosed { peer })
                .await?;
        }

        self.mgr_tx
            .send(TransportManagerEvent::ConnectionClosed { peer })
            .await
            .map_err(From::from)
    }

    /// Poll next substream open query from one of the installed protocols.
    pub async fn next_event(&mut self) -> Option<ProtocolCommand> {
        self.rx.recv().await
    }
}
