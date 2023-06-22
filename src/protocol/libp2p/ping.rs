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
    error::Error,
    peer_id::PeerId,
    protocol::{ConnectionEvent, ConnectionService, Direction},
    substream::Substream,
    types::protocol::ProtocolName,
    TransportService, DEFAULT_CHANNEL_SIZE,
};

use futures::{SinkExt, Stream, StreamExt};
use tokio::sync::mpsc::{channel, Sender};
use tokio_stream::wrappers::ReceiverStream;

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

// TODO: handle max failures
// TODO: don't block the event loop on a single substream

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::ping";

/// IPFS Ping protocol name as a string.
pub const PROTOCOL_NAME: &str = "/ipfs/ping/1.0.0";

/// Size for `/ipfs/ping/1.0.0` payloads.
const PING_PAYLOAD_SIZE: usize = 32;

/// Ping configuration.
// TODO: figure out a better abstraction for protocol configs
#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    pub(crate) protocol: ProtocolName,

    /// Codec used by the protocol.
    pub(crate) codec: ProtocolCodec,

    /// Maximum failures before the peer is considered unreachable.
    max_failures: usize,

    /// TX channel for sending events to the user protocol.
    tx_event: Sender<PingEvent>,
}

impl Config {
    /// Create new [`PingConfig`].
    ///
    /// Returns a config that is given to `Litep2pConfig` and an event stream for ping events.
    pub fn new(max_failures: usize) -> (Self, Box<dyn Stream<Item = PingEvent> + Send + Unpin>) {
        let (tx_event, rx_event) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                tx_event,
                max_failures,
                protocol: ProtocolName::from(PROTOCOL_NAME),
                codec: ProtocolCodec::Identity(PING_PAYLOAD_SIZE),
            },
            Box::new(ReceiverStream::new(rx_event)),
        )
    }
}

/// Events emitted by the ping protocol.
#[derive(Debug)]
pub enum PingEvent {
    /// Ping time with remote peer.
    Ping {
        /// Peer ID.
        peer: PeerId,

        /// Ping.
        ping: Duration,
    },
}

/// Ping protocol.
pub struct Ping {
    /// Maximum failures before the peer is considered unreachable.
    _max_failures: usize,

    // Connection service.
    service: TransportService,

    /// TX channel for sending events to the user protocol.
    tx: Sender<PingEvent>,

    /// Connected peers.
    peers: HashMap<PeerId, ConnectionService>,

    /// Pending outbound substreams.
    pending_outbound: HashMap<usize, PeerId>, // TODO: does this really need to be a hashmap?
}

impl Ping {
    /// Create new [`Ping`] protocol.
    pub fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            tx: config.tx_event,
            peers: HashMap::new(),
            pending_outbound: HashMap::new(),
            _max_failures: config.max_failures,
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId, mut service: ConnectionService) {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection established");

        match service.open_substream().await {
            Ok(substream_id) => {
                self.pending_outbound.insert(substream_id, peer);
                self.peers.insert(peer, service);
            }
            Err(_) => tracing::debug!(target: LOG_TARGET, ?peer, "connection closed"),
        }
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection closed");
        self.peers.remove(&peer);
    }

    /// Handle outbound substream.
    async fn on_outbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Substream>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle outbound substream");

        match substream.send(vec![0u8; 32].into()).await {
            Ok(_) => {
                let now = Instant::now();

                match substream.next().await {
                    Some(Ok(_)) => {
                        // suppress error as the user may have intentionally dropped the event stream
                        // which should not cause the the ping protocol to stop working.
                        let _ = self
                            .tx
                            .send(PingEvent::Ping {
                                peer,
                                ping: now.elapsed(),
                            })
                            .await;
                    }
                    Some(Err(error)) => tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?error,
                        "error while reading from the substream",
                    ),
                    None => {
                        tracing::debug!(target: LOG_TARGET, ?peer, "substream closed unexpectedly")
                    }
                }
            }
            Err(error) => tracing::debug!(
                target: LOG_TARGET,
                ?peer,
                ?error,
                "failed to send ping to remote peer"
            ),
        }
    }

    /// Substream opened to remote peer.
    async fn on_inbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Substream>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

        // TODO: don't block here
        match substream.next().await {
            Some(Ok(ping)) => {
                if let Err(error) = substream.send(ping.into()).await {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?error,
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
                ConnectionEvent::ConnectionEstablished { peer, service } => {
                    self.on_connection_established(peer, service).await
                }
                ConnectionEvent::ConnectionClosed { peer } => {
                    self.on_connection_closed(peer);
                }
                ConnectionEvent::SubstreamOpened {
                    peer,
                    substream,
                    direction,
                    ..
                } => match direction {
                    Direction::Inbound => self.on_inbound_substream(peer, substream).await,
                    Direction::Outbound(substream_id) => {
                        match self.pending_outbound.remove(&substream_id) {
                            Some(stored_peer) => {
                                debug_assert!(peer == stored_peer);
                                self.on_outbound_substream(peer, substream).await;
                            }
                            None => {
                                todo!("substream {substream_id} does not exist");
                            }
                        }
                    }
                },
                ConnectionEvent::SubstreamOpenFailure { peer, error } => {
                    self.on_substream_open_failure(peer, error);
                }
            }
        }
    }
}
