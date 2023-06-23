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
        notification::types::{NotificationCommand, NotificationEvent, ValidationResult},
        ConnectionEvent, ConnectionService, Direction,
    },
    substream::Substream,
    types::{protocol::ProtocolName, SubstreamId},
    TransportService,
};

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc::{Receiver, Sender};

use std::collections::{hash_map::Entry, HashMap};

pub mod types;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::protocol";

/// Peer state
#[derive(Debug)]
enum PeerState {
    /// Peer state is poisoned.
    Poisoned,

    /// Notification stream is closed.
    Closed,

    /// Outbound substream initiated.
    OutboundInitiated {
        /// Substream ID.
        substream_id: SubstreamId,
    },

    /// Outbound substream is open, waiting for inbound substream to open.
    OutboundOpen {
        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },

    /// Inbound substream is open, waiting for outbound substream to be accepted.
    InboundOpen {
        /// Inbound substream.
        inbound: Box<dyn Substream>,
    },

    /// Inbound substream has been accepted, outbound substream has been initiated.
    InboundOpenOutboundInitiated {
        /// ID for the outbound substream.
        substream_id: SubstreamId,

        /// Inbound substream.
        inbound: Box<dyn Substream>,
    },

    /// Notification stream is open.
    Open {
        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },
}

/// Peer context.
struct PeerContext {
    /// Connection service.
    service: ConnectionService,

    /// Peer state.
    state: PeerState,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new(service: ConnectionService) -> Self {
        Self {
            service,
            state: PeerState::Closed,
        }
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
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?substream_id,
            "handle outbound substream",
        );

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                ?substream_id,
                "peer doesn't exist",
            );
            debug_assert!(false);
            return Err(Error::PeerDoesntExist(peer));
        };

        // TODO: check if entry exists in `pending_outbound` for `substream_id`
        let _ = substream.send(self.handshake.clone().into()).await?;

        tracing::info!(target: LOG_TARGET, "handshake sent to {peer}");

        // TODO: don't block here
        match substream.next().await {
            Some(handshake) => match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                PeerState::OutboundInitiated { substream_id } => {
                    // TODO: verify that substream IDs match
                    context.state = PeerState::OutboundOpen {
                        outbound: substream,
                    };
                    Ok(())
                }
                PeerState::InboundOpenOutboundInitiated {
                    substream_id,
                    inbound,
                } => {
                    // TODO: verify that substream IDs match
                    context.state = PeerState::Open {
                        outbound: substream,
                    };
                    // TODO: store `inbound` to substream set
                    self.event_tx
                        .send(NotificationEvent::NotificationStreamOpened {
                            protocol: ProtocolName::from("/notif/1"),
                            peer,
                            handshake: handshake.unwrap().into(),
                        })
                        .await
                        .unwrap();
                    Ok(())
                }
                state => {
                    tracing::error!(target: LOG_TARGET, "invalid state for {peer:?}: {state:?}");
                    Ok(())
                }
            },
            None => {
                let _ = self
                    .event_tx
                    .send(NotificationEvent::NotificationStreamOpenFailure { peer })
                    .await;
                Err(Error::SubstreamError(SubstreamError::ConnectionClosed))
            }
        }
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        protocol: ProtocolName,
        peer: PeerId,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::error!(
                target: LOG_TARGET,
                ?protocol,
                ?peer,
                "peer doesn't exist",
            );
            debug_assert!(false);
            return Err(Error::PeerDoesntExist(peer));
        };

        // TODO: don't block here
        match substream.next().await {
            Some(Ok(handshake)) => match std::mem::replace(&mut context.state, PeerState::Poisoned)
            {
                PeerState::Closed => {
                    // TODO: don't ignore error
                    let _ = self
                        .event_tx
                        .send(NotificationEvent::ValidateSubstream {
                            protocol,
                            peer,
                            handshake: handshake.into(),
                        })
                        .await;
                    context.state = PeerState::InboundOpen { inbound: substream };
                    Ok(())
                }
                PeerState::OutboundOpen { outbound } => {
                    // TODO: handle error gracefuly
                    substream.send(self.handshake.clone().into()).await.unwrap();
                    context.state = PeerState::Open { outbound };
                    // TODO: handle error gracefuly
                    tracing::info!(target: LOG_TARGET, "notification stream opened for {peer}");

                    self.event_tx
                        .send(NotificationEvent::NotificationStreamOpened {
                            protocol,
                            peer,
                            handshake: vec![1, 2, 3, 4],
                        })
                        .await
                        .unwrap();
                    Ok(())
                }
                state => {
                    tracing::error!(target: LOG_TARGET, ?peer, ?state, "invalid state");
                    Ok(())
                }
            },
            Some(Err(error)) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?protocol,
                    ?peer,
                    ?error,
                    "failed to read handshake from inbound substream"
                );
                Err(Error::SubstreamError(SubstreamError::ConnectionClosed))
            }
            None => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?protocol,
                    ?peer,
                    "read `None` from inbound substream"
                );
                Err(Error::SubstreamError(SubstreamError::ConnectionClosed))
            }
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

    /// Open substream to remote `peer`.
    async fn on_open_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        match self.peers.get_mut(&peer) {
            None => Err(Error::PeerDoesntExist(peer)),
            Some(context) => {
                let substream_id = context.service.open_substream().await?;
                context.state = PeerState::OutboundInitiated { substream_id };
                Ok(())
            }
        }
    }

    /// Handle validation result.
    async fn on_validation_result(
        &mut self,
        peer: PeerId,
        result: ValidationResult,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?result,
            "handle validation result"
        );

        match self.peers.get_mut(&peer) {
            None => Err(Error::PeerDoesntExist(peer)),
            Some(context) => match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                PeerState::InboundOpen { mut inbound } => {
                    // TODO: handle error properly
                    inbound.send(self.handshake.clone().into()).await.unwrap();
                    let substream_id = context.service.open_substream().await?;
                    context.state = PeerState::InboundOpenOutboundInitiated {
                        substream_id,
                        inbound,
                    };
                    Ok(())
                }
                _ => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?peer,
                        state = ?context.state,
                        "invalid state for peer",
                    );
                    debug_assert!(false);
                    Ok(())
                }
            },
        }
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
                        protocol,
                    }) => match direction {
                        Direction::Inbound => {
                            if let Err(error) = self.on_inbound_substream(protocol, peer, substream).await {
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
                        NotificationCommand::OpenSubstream { peer } => {
                            if let Err(error) = self.on_open_substream(peer).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to open substream"
                                );
                            }
                        }
                        NotificationCommand::SubstreamValidated { peer, result } => {
                            if let Err(error) = self.on_validation_result(peer, result).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to open substream"
                                );
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
