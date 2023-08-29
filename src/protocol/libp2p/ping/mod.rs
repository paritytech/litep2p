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
    error::{Error, SubstreamError},
    peer_id::PeerId,
    protocol::{Direction, Transport, TransportEvent, TransportService},
    substream::Substream,
    types::SubstreamId,
};

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc::Sender;

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

pub use config::{PingConfig, PingConfigBuilder};

mod config;

// TODO: handle max failures
// TODO: don't block the event loop on a single substream

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::ping";

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
    peers: HashSet<PeerId>,

    /// Pending outbound substreams.
    pending_outbound: HashMap<SubstreamId, PeerId>, // TODO: does this really need to be a hashmap?
}

impl Ping {
    /// Create new [`Ping`] protocol.
    pub fn new(service: TransportService, config: PingConfig) -> Self {
        Self {
            service,
            tx: config.tx_event,
            peers: HashSet::new(),
            pending_outbound: HashMap::new(),
            _max_failures: config.max_failures,
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection established");

        let substream_id = self.service.open_substream(peer).await?;
        self.pending_outbound.insert(substream_id, peer);
        self.peers.insert(peer);

        Ok(())
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection closed");

        self.peers.remove(&peer);
    }

    /// Handle outbound substream.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle outbound substream");

        let _ = substream.send(vec![0u8; 32].into()).await?;
        let now = Instant::now();

        // TODO: not good, use `SubstreamSet`
        // TODO: verify payload
        let _ =
            substream
                .next()
                .await
                .ok_or(Error::SubstreamError(SubstreamError::ReadFailure(Some(
                    substream_id,
                ))))??;

        let res = self
            .tx
            .send(PingEvent::Ping {
                peer,
                ping: now.elapsed(),
            })
            .await
            .map_err(From::from);

        res
    }

    /// Substream opened to remote peer.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

        // TODO: don't block here
        let payload = substream
            .next()
            .await
            .ok_or(Error::SubstreamError(SubstreamError::ReadFailure(None)))??;

        substream.send(payload.into()).await
    }

    /// Failed to open substream to remote peer.
    fn on_substream_open_failure(&mut self, substream: SubstreamId, error: Error) {
        tracing::debug!(
            target: LOG_TARGET,
            ?substream,
            ?error,
            "failed to open substream"
        );
    }

    /// Start [`Ping`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting ping event loop");

        while let Some(event) = self.service.next_event().await {
            match event {
                TransportEvent::ConnectionEstablished { peer, .. } => {
                    if let Err(error) = self.on_connection_established(peer).await {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to register peer",
                        );
                    }
                }
                TransportEvent::ConnectionClosed { peer } => {
                    self.on_connection_closed(peer);
                }
                TransportEvent::SubstreamOpened {
                    peer,
                    substream,
                    direction,
                    ..
                } => match direction {
                    Direction::Inbound => {
                        if let Err(error) = self.on_inbound_substream(peer, substream).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to handle inbound substream",
                            );
                        }
                    }
                    Direction::Outbound(substream_id) => {
                        match self.pending_outbound.remove(&substream_id) {
                            Some(stored_peer) => {
                                debug_assert!(peer == stored_peer);
                                if let Err(error) = self
                                    .on_outbound_substream(peer, substream_id, substream)
                                    .await
                                {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?error,
                                        "failed to handle outbound substream",
                                    );
                                }
                            }
                            None => {
                                todo!("substream {substream_id:?} does not exist");
                            }
                        }
                    }
                },
                TransportEvent::SubstreamOpenFailure { substream, error } => {
                    self.on_substream_open_failure(substream, error);
                }
                TransportEvent::DialFailure { .. } => {}
            }
        }
    }
}
