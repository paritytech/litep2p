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
    error::SubstreamError,
    protocol::{Direction, TransportEvent, TransportService},
    substream::Substream,
    types::SubstreamId,
    PeerId,
};

use bytes::Bytes;
use futures::{
    stream::{self, BoxStream},
    FutureExt, StreamExt,
};
use rand::Rng as _;
use std::{
    collections::HashSet,
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
    /// Maximum failures before the peer is considered unreachable.
    /// This must be at least 1 until <https://github.com/paritytech/litep2p/pull/416> is adopted
    /// by the network. (With older litep2p every other ping fails.)
    // TODO: use this to disconnect peers.
    _max_failures: usize,

    /// Connection service.
    service: TransportService,

    /// TX channel for sending events to the user protocol.
    tx: mpsc::Sender<PingEvent>,

    /// Local pingers per peer.
    pingers: StreamMap<PeerId, BoxStream<'static, Result<Duration, PingError>>>,

    /// Substreams on which we retry pings after failure. Used for rate-limiting.
    retries: HashSet<SubstreamId>,

    /// Ping responders per peer.
    responders: StreamMap<PeerId, BoxStream<'static, Result<(), SubstreamError>>>,

    /// Interval between outbound pings.
    ping_interval: Duration,
}

impl Ping {
    /// Create new [`Ping`] protocol.
    pub fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            tx: config.tx_event,
            ping_interval: config.ping_interval,
            pingers: StreamMap::new(),
            retries: HashSet::new(),
            responders: StreamMap::new(),
            _max_failures: config.max_failures,
        }
    }

    /// Connection established to remote peer.
    fn on_connection_established(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        if let Err(error) = self.service.open_substream(peer) {
            tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to open substream");
        }
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");
    }

    /// Handle outbound substream.
    fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        substream: Substream,
    ) {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle outbound substream");
        let interval = self.ping_interval;
        let should_wait = self.retries.remove(&substream_id);

        let pinger_stream = stream::unfold(
            (substream, should_wait),
            move |(mut substream, should_wait)| async move {
                if should_wait {
                    tokio::time::sleep(interval).await;
                }

                let payload = Bytes::from(Vec::from(rand::thread_rng().gen::<[u8; 32]>()));

                let ping = async {
                    let now = Instant::now();

                    substream.send_framed(payload.clone()).await?;
                    let received = substream.next().await.ok_or(PingError::SubstreamError(
                        SubstreamError::ReadFailure(Some(substream_id)),
                    ))??;

                    if received == payload {
                        Ok(now.elapsed())
                    } else {
                        Err(PingError::InvalidPayload)
                    }
                };

                match tokio::time::timeout(Duration::from_secs(20), ping).await {
                    Ok(Ok(elapsed)) => Some((Ok(elapsed), (substream, true))),
                    Ok(Err(error)) => Some((Err(error), (substream, false))),
                    Err(timeout) => Some((Err(timeout.into()), (substream, false))),
                }
            },
        );

        let _prev = self.pingers.insert(peer, pinger_stream.boxed());
        debug_assert!(_prev.is_none());
    }

    /// Handle inbound substream.
    fn on_inbound_substream(&mut self, peer: PeerId, mut substream: Substream) {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

        let responder_future = async move {
            loop {
                if let Some(payload) = substream.next().await {
                    substream.send_framed(payload?.freeze()).await?;
                } else {
                    return Ok(());
                }
            }
        };

        if self.responders.insert(peer, responder_future.into_stream().boxed()).is_some() {
            tracing::trace!(
                target: LOG_TARGET,
                ?peer,
                "discarding ping substream as remote opened a new one",
            );
        }
    }

    /// Start [`Ping`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting ping event loop");

        loop {
            tokio::select! {
                event = self.service.next() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        self.on_connection_established(peer);
                    }
                    Some(TransportEvent::ConnectionClosed { peer }) => {
                        self.on_connection_closed(peer);
                    }
                    Some(TransportEvent::SubstreamOpened {
                        peer,
                        substream,
                        direction,
                        ..
                    }) => match direction {
                        Direction::Inbound => {
                            self.on_inbound_substream(peer, substream);
                        }
                        Direction::Outbound(substream_id) => {
                            self.on_outbound_substream(peer, substream_id, substream);
                        }
                    }
                    Some(TransportEvent::SubstreamOpenFailure {
                        substream,
                        ..
                    }) => {
                        self.retries.remove(&substream);
                    }
                    Some(_) => {}
                    None => return,
                },
                Some((peer, result)) = self.responders.next(), if !self.responders.is_empty() => {
                    // Remove the future from `StreamMap` to not wait untill it is polled again and
                    // removes it itself getting `None`. Otherwise we can get a confusing log
                    // message when try to insert a new responder for the same peer.
                    self.responders.remove(&peer);

                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        ?result,
                        "inbound ping responder terminated",
                    );
                }
                Some((peer, result)) = self.pingers.next(), if !self.pingers.is_empty() => {
                    match result {
                        Ok(elapsed) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                time_us = elapsed.as_micros(),
                                "pong",
                            );

                            let _ = self.tx.send(PingEvent::Ping { peer, ping: elapsed }).await;
                        }
                        Err(error) => {
                            self.pingers.remove(&peer);

                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "ping failed",
                            );

                            match self.service.open_substream(peer) {
                                Ok(substream_id) => {
                                    self.retries.insert(substream_id);
                                }
                                Err(error) => tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to open substream after ping failed",
                                ),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Possible error of the outbound ping.
#[derive(Debug, thiserror::Error)]
enum PingError {
    #[error("Substream error: {0}")]
    SubstreamError(#[from] SubstreamError),
    #[error("Invalid payload received")]
    InvalidPayload,
    #[error("Timeout")]
    Timeout(#[from] tokio::time::error::Elapsed),
}
