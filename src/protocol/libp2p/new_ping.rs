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
    codec::{identity::Identity, Codec},
    error::Error,
    new::ConnectionService,
    peer_id::PeerId,
    protocol::{ConnectionEvent, ProtocolEvent},
    substream::{Substream, SubstreamSet},
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use futures::{AsyncReadExt, AsyncWriteExt, SinkExt, Stream, StreamExt};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;

use std::collections::HashMap;

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::ping";

/// IPFS Ping protocol name as a string.
pub const PROTOCOL_NAME: &str = "/ipfs/ping/1.0.0";

/// Size for `/ipfs/ping/1.0.0` payloads.
const PING_PAYLOAD_SIZE: usize = 32;

/// Ping configuration.
#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    pub(crate) protocol: ProtocolName,

    /// Maximum failures before the peer is considered unreachable.
    max_failures: usize,

    /// TX channel for sending events to the user protocol.
    tx_event: Sender<PingEvent>,
}

impl Config {
    /// Create new [`PingConfig`].
    ///
    /// Returns a config that is given to `Litep2pConfig` and an event stream for ping events.
    pub fn new(max_failures: usize) -> (Self, Box<dyn Stream<Item = PingEvent> + Send>) {
        let (tx_event, rx_event) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                tx_event,
                max_failures,
                protocol: ProtocolName::from(PROTOCOL_NAME),
            },
            Box::new(ReceiverStream::new(rx_event)),
        )
    }
}

/// Events emitted by the ping protocol.
#[derive(Debug)]
pub enum PingEvent {}

/// Ping protocol.
pub struct Ping {
    /// Maximum failures before the peer is considered unreachable.
    max_failures: usize,

    // Connection service.
    service: ConnectionService,

    /// TX channel for sending events to the user protocol.
    tx: Sender<PingEvent>,

    /// Connected peers.
    peers: HashMap<PeerId, Sender<ProtocolEvent>>,
}

impl Ping {
    /// Create new [`Ping`] protocol.
    pub fn new(service: ConnectionService, config: Config) -> Self {
        Self {
            service,
            tx: config.tx_event,
            peers: HashMap::new(),
            max_failures: config.max_failures,
        }
    }

    /// Connection established to remote peer.
    fn on_connection_established(&mut self, peer: PeerId, connection: Sender<ProtocolEvent>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection established");
        self.peers.insert(peer, connection);
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection closed");
        self.peers.remove(&peer);
    }

    /// Substream opened to remote peer.
    async fn on_substream_opened(&mut self, peer: PeerId, mut substream: Box<dyn Substream>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "substream opened");

        // TODO: don't block here
        match substream.next().await {
            Some(Ok(ping)) => {
                if let Err(error) = substream.send(ping).await {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        "failed to write value back to sender"
                    );
                }
            }
            Some(Err(error)) => tracing::debug!(
                target: LOG_TARGET,
                ?peer,
                ?error,
                "error while reading from the substream",
            ),
            None => tracing::debug!(target: LOG_TARGET, ?peer, "substream closed unexpectedly"),
        }
    }

    /// Failed to open substream to remote peer.
    fn on_substream_open_failure(&mut self, peer: PeerId, error: Error) {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            ?error,
            "failed to open substream"
        );
    }

    /// Start [`Ping`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting ping event loop");

        while let Some(event) = self.service.next_event().await {
            match event {
                ConnectionEvent::ConnectionEstablished { peer, connection } => {
                    self.on_connection_established(peer, connection)
                }
                ConnectionEvent::ConnectionClosed { peer } => {
                    self.on_connection_closed(peer);
                }
                ConnectionEvent::SubstreamOpened { peer, substream } => {
                    self.on_substream_opened(peer, substream).await;
                }
                ConnectionEvent::SubstreamOpenFailure { peer, error } => {
                    self.on_substream_open_failure(peer, error);
                }
            }
        }
    }
}
