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
    protocol::{ConnectionEvent, ProtocolEvent, ProtocolInfo},
    types::{protocol::ProtocolName, ConnectionId},
    DEFAULT_CHANNEL_SIZE,
};

use multiaddr::Multiaddr;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{collections::HashMap, fmt::Debug};

pub mod quic;
pub mod tcp;

/// Commands send by `Litep2p` to the transport.
#[derive(Debug)]
pub(crate) enum TransportCommand {
    /// Dial remote peer at `address`.
    Dial {
        /// Address.
        address: Multiaddr,

        /// Connection ID allocated for this connection.
        connection_id: ConnectionId,
    },
}

/// Events emitted by the underlying transport.
#[derive(Debug)]
pub enum TransportEvent {
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Remote address.
        address: Multiaddr,
    },

    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
    },

    DialFailure {
        /// Dialed address.
        address: Multiaddr,

        /// Error.
        error: Error,
    },
}

#[async_trait::async_trait]
pub(crate) trait Transport {
    type Config: Debug;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self>
    where
        Self: Sized;

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr;

    /// Start transport event loop.
    async fn start(mut self) -> crate::Result<()>;
}

#[derive(Debug)]
pub struct TransportService {
    rx: Receiver<ConnectionEvent>,
    _peers: HashMap<PeerId, Sender<ProtocolEvent>>,
}

impl TransportService {
    /// Create new [`ConnectionService`].
    pub fn new() -> (Self, Sender<ConnectionEvent>) {
        // TODO: maybe specify some other channel size
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                rx,
                _peers: HashMap::new(),
            },
            tx,
        )
    }

    /// Get next event from the transport.
    pub async fn next_event(&mut self) -> Option<ConnectionEvent> {
        self.rx.recv().await
    }
}

/// Transport context.
#[derive(Debug, Clone)]
pub struct TransportContext {
    /// Enabled protocols.
    pub(crate) protocols: HashMap<ProtocolName, ProtocolInfo>,

    /// Keypair.
    pub(crate) keypair: Keypair,

    /// TX channel for sending events to [`Litep2p`].
    tx: Sender<TransportEvent>,
}

impl TransportContext {
    /// Create new [`TransportContext`].
    pub fn new(keypair: Keypair, tx: Sender<TransportEvent>) -> Self {
        Self {
            tx,
            keypair,
            protocols: HashMap::new(),
        }
    }

    /// Add new protocol.
    pub fn add_protocol(
        &mut self,
        protocol: ProtocolName,
        codec: ProtocolCodec,
    ) -> crate::Result<TransportService> {
        let (service, tx) = TransportService::new();

        match self
            .protocols
            .insert(protocol.clone(), ProtocolInfo { tx, codec })
        {
            Some(_) => Err(Error::ProtocolAlreadyExists(protocol)),
            None => Ok(service),
        }
    }

    /// Report to `Litep2p` that a peer connected.
    pub(crate) async fn report_connection_established(&mut self, peer: PeerId, address: Multiaddr) {
        let _ = self
            .tx
            .send(TransportEvent::ConnectionEstablished { peer, address })
            .await;
    }

    /// Report to `Litep2p` that a peer disconnected.
    pub(crate) async fn report_connection_closed(&mut self, peer: PeerId) {
        let _ = self
            .tx
            .send(TransportEvent::ConnectionClosed { peer })
            .await;
    }

    /// Report to `Litep2p` that dialing a remote peer failed.
    pub(crate) async fn report_dial_failure(&mut self, address: Multiaddr, error: Error) {
        let _ = self
            .tx
            .send(TransportEvent::DialFailure { address, error })
            .await;
    }
}
