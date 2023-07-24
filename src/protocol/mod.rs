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
    error::Error,
    peer_id::PeerId,
    substream::{RawSubstream, Substream, SubstreamType},
    transport::{TransportContext, TransportService},
    types::{protocol::ProtocolName, SubstreamId},
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_util::codec::Framed;

use std::{collections::HashMap, fmt::Debug};

pub mod libp2p;
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

/// Events emitted by a connection to protocols.
pub enum ConnectionEvent {
    /// Connection established to `peer`.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Handle for communicating with the connection.
        service: ConnectionService,
    },

    /// Connection closed.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
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

/// Events emitted by the installed protocols to transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolEvent {
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

// TODO: move this somewhere else
/// Service provided to protocols by the transport protocol.
#[derive(Debug, Clone)]
pub struct ConnectionService {
    /// TX channel for sending events to transport.
    tx: Sender<ProtocolEvent>,

    /// Protocol name.
    protocol: ProtocolName,

    /// Next ephemeral substream ID.
    next_substream_id: SubstreamId,
}

// TODO: move this somewhere else
impl ConnectionService {
    /// Create new [`ConnectionService`].
    pub fn new(protocol: ProtocolName, tx: Sender<ProtocolEvent>) -> Self {
        Self {
            tx,
            protocol,
            next_substream_id: 0usize,
        }
    }

    /// Get next ephemeral substream ID.
    fn next_substream_id(&mut self) -> SubstreamId {
        let substream_id = self.next_substream_id;
        self.next_substream_id += 1;
        substream_id
    }

    /// Open substream to remote peer over `protocol`.
    pub async fn open_substream(&mut self) -> crate::Result<SubstreamId> {
        let substream_id = self.next_substream_id();

        tracing::trace!(target: LOG_TARGET, ?substream_id, "open substream");

        self.tx
            .send(ProtocolEvent::OpenSubstream {
                protocol: self.protocol.clone(),
                substream_id,
            })
            .await
            .map(|_| substream_id)
            .map_err(From::from)
    }
}

// TODO: move this somewhere else
/// Protocol information.
#[derive(Debug, Clone)]
pub(crate) struct ProtocolInfo {
    /// TX channel for sending connection events to the protocol.
    pub tx: Sender<ConnectionEvent>,

    /// Codec used by the protocol.
    pub codec: ProtocolCodec,
}

// TODO: move this somewhere else
/// Supported protocol information.
///
/// Each connection gets a copy of [`ProtocolSet`] which allows it to interact
/// directly with installed protocols.
#[derive(Debug)]
pub struct ProtocolSet {
    /// Installed protocols.
    pub(crate) protocols: HashMap<ProtocolName, ProtocolInfo>,

    /// RX channel for protocol events.
    rx: Receiver<ProtocolEvent>,
}

// TODO: move this somewhere else
impl ProtocolSet {
    /// Create new [`ProtocolSet`] and transfer `ConnectionEstablished` to all installed protocols.
    pub async fn from_transport_context(
        peer: PeerId,
        context: TransportContext,
    ) -> crate::Result<Self> {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            "connection established to remote peer"
        );

        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);

        // TODO: this is kind of ugly
        // TODO: backpressure?
        for (protocol, sender) in &context.protocols {
            sender
                .tx
                .send(ConnectionEvent::ConnectionEstablished {
                    peer,
                    service: ConnectionService::new(protocol.clone(), tx.clone()),
                })
                .await?;
        }

        Ok(Self {
            rx,
            protocols: context.protocols,
        })
    }

    /// Report to `protocol` that substream was opened for `peer`.
    pub async fn report_substream_open<R: RawSubstream>(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        direction: Direction,
        substream: SubstreamType<R>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?peer, "substream opened");

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
                    SubstreamType::ChannelBackend(substream) => Box::new(substream),
                };

                info.tx
                    .send(ConnectionEvent::SubstreamOpened {
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
                .send(ConnectionEvent::SubstreamOpenFailure { substream, error })
                .await
                .map_err(From::from),
            None => Err(Error::ProtocolNotSupported(protocol.to_string())),
        }
    }

    /// Poll next substream open query from one of the installed protocols.
    pub async fn next_event(&mut self) -> Option<ProtocolEvent> {
        self.rx.recv().await
    }
}
