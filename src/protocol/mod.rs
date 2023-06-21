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
    substream::{RawSubstream, Substream},
    types::protocol::ProtocolName,
    ProtocolInfo, TransportContext,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_util::codec::Framed;

use std::{collections::HashMap, fmt::Debug};

pub mod libp2p;
pub mod notification;
pub mod request_response;

const LOG_TARGET: &str = "protocol";

/// Events emitted by a connection to protocols.
pub enum ConnectionEvent {
    /// Connection established to `peer`.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Handle for communicating with the connection.
        connection: Sender<ProtocolEvent>,
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
        /// and `/ipfs/identify/push/1.0.0`) or the it may have aliases which should be handled by
        /// the same protocol handler. When the substream is sent from transpor to the protocol
        /// handler, the protocol name that was used to negotiate the substream is also sent so
        /// the protocol can handle the substream appropriately.
        protocol: ProtocolName,

        /// Substream.
        substream: Box<dyn Substream>,
    },

    /// Failed to open substream.
    SubstreamOpenFailure {
        /// Peer Id.
        peer: PeerId,

        /// Error.
        error: Error,
    },
}

/// Events emitted by the installed protocols to transport.
#[derive(Debug)]
pub enum ProtocolEvent {
    /// Open substream.
    OpenSubstream {
        /// Protocol name.
        protocol: ProtocolName,
    },
}

/// Supported protocol information.
///
/// Each connection gets a copy of [`ProtocolSet`] which allows it to interact
/// directly with installed protocols.
#[derive(Debug)]
pub struct ProtocolSet {
    pub protocols: HashMap<ProtocolName, ProtocolInfo>,
    rx: Receiver<ProtocolEvent>,
}

impl ProtocolSet {
    /// Create new [`ProtocolSet`] and transfer `ConnectionEstablished` to all installed protocols.
    pub async fn from_transport_context(
        peer: PeerId,
        context: TransportContext,
    ) -> crate::Result<Self> {
        let (tx, rx) = channel(64);

        // TODO: this is kind of ugly
        // TODO: backpressure?
        for sender in context.protocols.values() {
            sender
                .tx
                .send(ConnectionEvent::ConnectionEstablished {
                    peer,
                    connection: tx.clone(),
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
        protocol: ProtocolName,
        peer: PeerId,
        substream: R,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?peer, "substream opened");

        match self.protocols.get_mut(&protocol) {
            Some(info) => {
                let substream: Box<dyn Substream> = match info.codec {
                    ProtocolCodec::Identity(payload_size) => {
                        Box::new(Framed::new(substream, Identity::new(payload_size)))
                    }
                    ProtocolCodec::UnsignedVarint => {
                        Box::new(Framed::new(substream, UnsignedVarint::new()))
                    }
                };

                info.tx
                    .send(ConnectionEvent::SubstreamOpened {
                        peer,
                        protocol: protocol.clone(),
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
        peer: PeerId,
        error: Error,
    ) -> crate::Result<()> {
        match self.protocols.get_mut(&protocol) {
            Some(info) => info
                .tx
                .send(ConnectionEvent::SubstreamOpenFailure { peer, error })
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
