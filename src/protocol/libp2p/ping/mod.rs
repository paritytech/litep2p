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
    error::{Error, SubstreamError},
    protocol::{Direction, TransportEvent, TransportService},
    substream::Substream,
    PeerId,
};

use futures::StreamExt;
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use tokio::sync::mpsc;

pub use config::{Config, ConfigBuilder};
mod config;

// TODO: https://github.com/paritytech/litep2p/issues/132 let the user handle max failures

/// Log target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::ping";
const PING_TIMEOUT: Duration = Duration::from_secs(10);

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
    Failure {
        peer: PeerId,
    },
}

enum PingCommand {
    SendPing,
}

enum PingResult {
    Success(Duration),
    Failure,
}

enum PeerState {
    Pending,
    Active {
        command_tx: mpsc::Sender<PingCommand>,
        failures: usize,
    },
}

/// Ping protocol.
pub(crate) struct Ping {
    /// Maximum failures before the peer is considered unreachable.
    max_failures: usize,

    // Connection service.
    service: TransportService,

    /// TX channel for sending events to the user protocol.
    tx: mpsc::Sender<PingEvent>,

    /// Connected peers.
    peers: HashMap<PeerId, PeerState>,

    ping_interval: Duration,

    result_rx: mpsc::Receiver<(PeerId, PingResult)>,
    result_tx: mpsc::Sender<(PeerId, PingResult)>,
}

impl Ping {
    /// Create new [`Ping`] protocol.
    pub fn new(service: TransportService, config: Config) -> Self {
        let (result_tx, result_rx) = mpsc::channel(256);
        Self {
            service,
            tx: config.tx_event,
            peers: HashMap::new(),
            ping_interval: config.ping_interval,
            max_failures: config.max_failures,
            result_rx,
            result_tx,
        }
    }

    /// Connection established to remote peer.
    fn on_connection_established(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established, opening ping substream");

        match self.service.open_substream(peer) {
            Ok(_) => {
                self.peers.insert(peer, PeerState::Pending);
            }
            Err(error) => {
                tracing::warn!(target: LOG_TARGET, ?peer, ?error, "failed to open ping substream");
            }
        }
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");
        self.peers.remove(&peer);
    }

    /// Handle outbound substream.
    fn on_outbound_substream(&mut self, peer: PeerId, substream: Substream) {
        tracing::trace!(target: LOG_TARGET, ?peer, "outbound ping substream opened");

        if let Some(PeerState::Pending) = self.peers.get(&peer) {
            let (command_tx, command_rx) = mpsc::channel(1);
            let result_tx = self.result_tx.clone();

            tokio::spawn(handle_ping_substream(
                peer,
                substream,
                command_rx,
                result_tx,
            ));

            self.peers.insert(
                peer,
                PeerState::Active {
                    command_tx,
                    failures: 0,
                },
            );
        } else {
            tracing::warn!(target: LOG_TARGET, ?peer, "ping substream opened for non-pending peer");
        }
    }

    /// Substream opened to remote peer.
    fn on_inbound_substream(&mut self, peer: PeerId, substream: Substream) {
        tracing::trace!(target: LOG_TARGET, ?peer, "handling inbound ping substream");
        tokio::spawn(handle_inbound_ping(substream));
    }

    async fn on_ping_result(&mut self, peer: PeerId, result: PingResult) {
        match self.peers.get_mut(&peer) {
            Some(PeerState::Active { failures, .. }) => match result {
                PingResult::Success(duration) => {
                    *failures = 0;
                    let _ = self.tx.send(PingEvent::Ping { peer, ping: duration }).await;
                }
                PingResult::Failure => {
                    *failures += 1;
                    tracing::debug!(target: LOG_TARGET, ?peer, failures, "ping failure");

                    if *failures >= self.max_failures {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?peer,
                            "maximum ping failures reached, closing connection"
                        );
                        let _ = self.tx.send(PingEvent::Failure { peer }).await;
                        if let Err(e) = self.service.force_close(peer) {
                            tracing::error!(target: LOG_TARGET, ?peer, ?e, "failed to force close connection");
                        }
                        self.peers.remove(&peer);
                    }
                }
            },
            _ => {
                tracing::trace!(target: LOG_TARGET, ?peer, "ping result for inactive peer");
            }
        }
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
                    Some(TransportEvent::SubstreamOpened { peer, substream, direction, .. }) => match direction {
                        Direction::Outbound(_) => self.on_outbound_substream(peer, substream),
                        Direction::Inbound => self.on_inbound_substream(peer, substream),
                    }
                    Some(_) => {}
                    None => {
                        tracing::debug!(target: LOG_TARGET, "transport service shut down");
                        return;
                    }
                },
                _ = interval.tick() => {
                    for (peer, state) in self.peers.iter() {
                        if let PeerState::Active { command_tx, .. } = state {
                            if let Err(e) = command_tx.try_send(PingCommand::SendPing) {
                                tracing::trace!(target: LOG_TARGET, ?peer, ?e, "failed to send ping command");
                            }
                        }
                    }
                },
                Some((peer, result)) = self.result_rx.recv() => {
                    self.on_ping_result(peer, result).await;
                }
            }
        }
    }
}

async fn handle_ping_substream(
    peer: PeerId,
    mut substream: Substream,
    mut command_rx: mpsc::Receiver<PingCommand>,
    result_tx: mpsc::Sender<(PeerId, PingResult)>,
) {
    loop {
        match command_rx.recv().await {
            Some(PingCommand::SendPing) => {
                // TODO: https://github.com/paritytech/litep2p/issues/134 generate random payload and verify it
                let payload = vec![0u8; 32];
                let future = async {
                    substream.send_framed(payload.into()).await?;
                    let now = Instant::now();
                    let _ = substream
                        .next()
                        .await
                        .ok_or(Error::SubstreamError(SubstreamError::ConnectionClosed))??;
                    Ok::<_, Error>(now.elapsed())
                };

                match tokio::time::timeout(PING_TIMEOUT, future).await {
                    Ok(Ok(duration)) => {
                        if result_tx.send((peer, PingResult::Success(duration))).await.is_err() {
                            break;
                        }
                    }
                    _ => {
                        if result_tx.send((peer, PingResult::Failure)).await.is_err() {
                            break;
                        }
                    }
                }
            }
            None => {
                tracing::trace!(target: LOG_TARGET, ?peer, "ping command channel closed, shutting down task");
                break;
            }
        }
    }
}

async fn handle_inbound_ping(mut substream: Substream) {
    while let Some(Ok(payload)) = substream.next().await {
        if substream.send_framed(payload.freeze()).await.is_err() {
            break;
        }
    }
}