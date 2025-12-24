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

//! [`/ipfs/ping/1.0.0`](https://github.com/libp2p/specs/blob/master/ping/ping.md) implementation.

use crate::{
    protocol::{Direction, TransportEvent, TransportService},
    substream::Substream,
    PeerId,
};

use bytes::Bytes;
use futures::{stream::SplitSink, SinkExt, StreamExt};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;
use tokio_stream::StreamMap;

pub use config::{Config, ConfigBuilder};
mod config;

// TODO: https://github.com/paritytech/litep2p/issues/132 let the user handle max failures

/// Log target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::ping";

/// Events emitted by the ping protocol.
#[derive(Debug)]
pub enum PingEvent {
    /// Ping time with remote peer.
    Ping {
        /// Peer ID.
        peer: PeerId,

        /// Measured ping time with the peer.
        ping: Duration,
    },
}

/// Ping protocol.
pub(crate) struct Ping {
    // Connection service.
    service: TransportService,

    /// TX channel for sending events to the user protocol.
    tx: mpsc::Sender<PingEvent>,

    /// Streams we read Pongs from.
    outbound_streams: StreamMap<PeerId, futures::stream::SplitStream<Substream>>,

    /// Sinks we write Pings to.
    outbound_sinks: HashMap<PeerId, SplitSink<Substream, Bytes>>,

    /// Streams we read Pings from.
    /// Keyed by PeerId which enforces one stream per peer
    inbound_streams: StreamMap<PeerId, futures::stream::SplitStream<Substream>>,

    /// Sinks we write Pongs to.
    inbound_sinks: HashMap<PeerId, SplitSink<Substream, Bytes>>,
    /// We need to track when we sent the ping to calculate the duration.
    ping_times: HashMap<PeerId, Instant>,

    ping_interval: Duration,
}

impl Ping {
    /// Create new [`Ping`] protocol.
    pub fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            tx: config.tx_event,
            ping_interval: config.ping_interval,
            outbound_streams: StreamMap::new(),
            outbound_sinks: HashMap::new(),
            ping_times: HashMap::new(),
            inbound_streams: StreamMap::new(),
            inbound_sinks: HashMap::new(),
        }
    }

    /// Connection established to remote peer.
    fn on_connection_established(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established, opening ping substream");

        if let Err(error) = self.service.open_substream(peer) {
            tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to open substream");
        }
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");
        self.outbound_streams.remove(&peer);
        self.outbound_sinks.remove(&peer);
        self.ping_times.remove(&peer);

        self.inbound_streams.remove(&peer);
        self.inbound_sinks.remove(&peer);
    }

    /// Handle outbound substream (We initiated)
    /// Registers it into the Outbound pipeline.
    fn on_outbound_substream(&mut self, peer: PeerId, substream: Substream) {
        tracing::trace!(target: LOG_TARGET, ?peer, "outbound ping substream registered");
        let (sink, stream) = substream.split();
        self.outbound_streams.insert(peer, stream);
        self.outbound_sinks.insert(peer, sink);
    }

    /// Handle inbound substream (They initiated).
    /// Registers it into the Inbound pipeline.
    fn on_inbound_substream(&mut self, peer: PeerId, substream: Substream) {
        tracing::trace!(target: LOG_TARGET, ?peer, "inbound ping substream registered");
        let (sink, stream) = substream.split();

        self.inbound_streams.insert(peer, stream);
        self.inbound_sinks.insert(peer, sink);
    }

    /// Start [`Ping`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting ping event loop");
        let mut interval = tokio::time::interval(self.ping_interval);

        loop {
            tokio::select! {
                event = self.service.next() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        self.on_connection_established(peer);
                    }
                    Some(TransportEvent::ConnectionClosed { peer }) => {
                        self.on_connection_closed(peer);
                    }
                    Some(TransportEvent::SubstreamOpened { peer, substream, direction,.. }) => match direction {
                        Direction::Inbound => {
                            self.on_inbound_substream(peer, substream);
                        }
                        Direction::Outbound(_) => {
                            self.on_outbound_substream(peer, substream);
                        }
                    }
                    Some(_) => {}
                    None => return,
                },

                _ = interval.tick() => {
                    for (peer, sink) in self.outbound_sinks.iter_mut() {
                        // TODO: https://github.com/paritytech/litep2p/issues/134 generate random payload and verify it
                        let payload = vec![0u8; 32];

                        tracing::trace!(target: LOG_TARGET, ?peer, "sending ping");

                        if let Err(error) = sink.send(Bytes::from(payload)).await {
                            tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to send ping");
                        } else {
                            self.ping_times.insert(*peer, Instant::now());
                        }
                    }
                }

                // Handle Outbound Responses (Pong is expected here)
                Some((peer, event)) = self.outbound_streams.next() => {
                    match event {
                        Ok(_payload) => {
                             if let Some(started) = self.ping_times.remove(&peer) {

                                 let elapsed = started.elapsed();
                                 tracing::trace!(target: LOG_TARGET, ?peer, ?elapsed, "pong received");
                                 let _ = self.tx.send(PingEvent::Ping { peer, ping: elapsed }).await;
                             }
                        }
                        Err(error) => {
                            tracing::debug!(target: LOG_TARGET, ?peer, ?error, "ping substream closed/error");
                            self.outbound_streams.remove(&peer);
                            self.outbound_sinks.remove(&peer);
                            self.ping_times.remove(&peer);
                        }
                    }
                }

                // Handle Inbound Pings
                Some((peer, event)) = self.inbound_streams.next() => {
                    match event {
                        Ok(payload) => {
                             if let Some(sink) = self.inbound_sinks.get_mut(&peer) {
                                 tracing::trace!(target: LOG_TARGET, ?peer, "sending pong");
                                 if let Err(error) = sink.send(payload.freeze()).await {
                                     tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to send pong");
                                 }
                             } else {
                                 tracing::debug!(
                                     target: LOG_TARGET,
                                     ?peer,
                                     "received ping from peer but no sink available to reply"
                                 );
                             }
                        }
                        Err(error) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "inbound ping substream error"
                            );
                            self.inbound_streams.remove(&peer);
                            self.inbound_sinks.remove(&peer);
                        }
                    }
                }
            }
        }
    }
}
