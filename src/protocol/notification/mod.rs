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
    protocol::{
        notification::types::{NotificationCommand, NotificationEvent},
        ConnectionEvent, ConnectionService, Direction,
    },
    substream::Substream,
    types::SubstreamId,
    TransportService,
};

use tokio::sync::mpsc::{Receiver, Sender};

use std::collections::{hash_map::Entry, HashMap};

pub mod types;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::protocol";

/// Peer context.
struct PeerContext {
    /// Connection service.
    service: ConnectionService,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new(service: ConnectionService) -> Self {
        Self { service }
    }
}

pub struct NotificationProtocol {
    /// Transport service.
    service: TransportService,

    /// Handshake bytes.
    handshake: Vec<u8>,

    /// TX channel passed to the protocol used for sending events.
    event_tx: Sender<NotificationEvent>,

    /// RX channel passed to the protocol used for receiving commands.
    command_rx: Receiver<NotificationCommand>,

    /// Connected peers.
    peers: HashMap<PeerId, PeerContext>,
}

impl NotificationProtocol {
    pub fn new(service: TransportService, config: types::Config) -> Self {
        Self {
            service,
            peers: HashMap::new(),
            handshake: config.handshake,
            event_tx: config.event_tx,
            command_rx: config.command_rx,
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(
        &mut self,
        peer: PeerId,
        service: ConnectionService,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                entry.insert(PeerContext::new(service));
                Ok(())
            }
            Entry::Occupied(_) => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?peer,
                    "state mismatch: peer already exists"
                );
                debug_assert!(false);
                Err(Error::PeerAlreadyExists(peer))
            }
        }
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        if let None = self.peers.remove(&peer) {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                "state mismatch: peer doesn't exist"
            );
            debug_assert!(false);
        }
    }

    /// Local node opened a substream to remote node.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        _substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?substream_id,
            "handle outbound substream",
        );

        Ok(())
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        _substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

        Ok(())
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

    /// Start [`NotificationProtocol`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting request-response event loop");

        loop {
            tokio::select! {
                event = self.service.next_event() => match event {
                    Some(ConnectionEvent::ConnectionEstablished { peer, service }) => {
                        if let Err(error) = self.on_connection_established(peer, service).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to register peer",
                            );
                        }
                    }
                    Some(ConnectionEvent::ConnectionClosed { peer }) => {
                        self.on_connection_closed(peer);
                    }
                    Some(ConnectionEvent::SubstreamOpened {
                        peer,
                        substream,
                        direction,
                        ..
                    }) => match direction {
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
                    },
                    Some(ConnectionEvent::SubstreamOpenFailure { peer, error }) => {
                        self.on_substream_open_failure(peer, error);
                    }
                    None => return,
                },
                command = self.command_rx.recv() => match command {
                    None => {
                        tracing::debug!(target: LOG_TARGET, "user protocol has exited, exiting");
                        return
                    }
                    Some(command) => match command {
                        _ => {}
                    }
                }
            }
        }
    }
}
