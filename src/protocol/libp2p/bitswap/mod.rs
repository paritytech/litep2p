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

#![allow(unused)]

use crate::{
    error::{Error, SubstreamError},
    peer_id::PeerId,
    protocol::{Direction, Transport, TransportEvent, TransportService},
    substream::Substream,
    types::SubstreamId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, SinkExt, StreamExt};
use tokio::sync::mpsc::Sender;

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

pub use config::BitswapConfig;
pub use handle::{BitswapEvent, BitswapHandle};

mod config;
mod handle;

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::bitswap";

/// Bitswap protocol.
pub struct Bitswap {
    // Connection service.
    service: TransportService,

    /// TX channel for sending events to the user protocol.
    tx: Sender<BitswapEvent>,

    /// Connected peers.
    peers: HashSet<PeerId>,

    /// Pending outbound substreams.
    pending_opens: HashMap<SubstreamId, PeerId>,

    /// Pending outbound substreams.
    pending_outbound: FuturesUnordered<BoxFuture<'static, ()>>,

    /// Pending inbound substreams.
    pending_inbound: FuturesUnordered<BoxFuture<'static, ()>>,
}

impl Bitswap {
    /// Create new [`Ping`] protocol.
    pub fn new(service: TransportService, config: BitswapConfig) -> Self {
        Self {
            service,
            tx: config.event_tx,
            peers: HashSet::new(),
            pending_opens: HashMap::new(),
            pending_outbound: FuturesUnordered::new(),
            pending_inbound: FuturesUnordered::new(),
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection established");

        Ok(())
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection closed");
    }

    /// Handle outbound substream.
    fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        mut substream: Box<dyn Substream>,
    ) {
    }

    /// Substream opened to remote peer.
    fn on_inbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Substream>) {
        tracing::warn!(target: LOG_TARGET, ?peer, "handle inbound substream");
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
        tracing::debug!(target: LOG_TARGET, "starting bitswap event loop");

        loop {
            tokio::select! {
                event = self.service.next_event() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        if let Err(error) = self.on_connection_established(peer).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to register peer",
                            );
                        }
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
                            match self.pending_opens.remove(&substream_id) {
                                Some(stored_peer) => {
                                    debug_assert!(peer == stored_peer);
                                    self.on_outbound_substream(peer, substream_id, substream);
                                }
                                None => {
                                    todo!("substream {substream_id:?} does not exist");
                                }
                            }
                        }
                    },
                    Some(TransportEvent::SubstreamOpenFailure { substream, error }) => {
                        self.on_substream_open_failure(substream, error);
                    }
                    Some(TransportEvent::DialFailure { .. }) => {}
                    None => return,
                },
                _event = self.pending_inbound.next(), if !self.pending_inbound.is_empty() => {}
                event = self.pending_outbound.next(), if !self.pending_outbound.is_empty() => {
                    match event {
                        Some(_) => {}
                        event => tracing::trace!(
                            target: LOG_TARGET,
                            ?event,
                            "failed to handle bitswap for an outbound peer"
                        ),
                    }
                }
            }
        }
    }
}
