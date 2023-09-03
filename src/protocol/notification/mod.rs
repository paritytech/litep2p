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

//! Notification protocol implementation.

use crate::{
    error::Error,
    protocol::{
        notification::{
            handle::{NotificationEventHandle, NotificationSink},
            negotiation::{HandshakeEvent, HandshakeService},
            types::{NotificationCommand, ASYNC_CHANNEL_SIZE, SYNC_CHANNEL_SIZE},
        },
        Direction, Transport, TransportEvent, TransportService,
    },
    substream::{Substream, SubstreamSet},
    types::{protocol::ProtocolName, SubstreamId},
    PeerId,
};

use bytes::BytesMut;
use futures::{
    stream::{select, Select},
    SinkExt, StreamExt,
};
use tokio::sync::mpsc::{channel, Receiver};
use tokio_stream::{wrappers::ReceiverStream, StreamMap};

use std::collections::{hash_map::Entry, HashMap};

pub use config::{Config, ConfigBuilder};
pub use handle::NotificationHandle;
pub use types::{NotificationError, NotificationEvent, ValidationResult};

mod config;
mod handle;
mod negotiation;
mod types;

#[cfg(test)]
mod tests;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::protocol";

/// Inbound substream state.
#[derive(Debug)]
enum InboundState {
    /// Substream is closed.
    Closed,

    /// Handshake is being read from the remote peer.
    ReadingHandshake,

    /// Substream and its handshake are being validated by the user protocol.
    Validating {
        /// Inbound substream.
        inbound: Box<dyn Substream>,
    },

    /// Handshake is being sent to the remote peer.
    SendingHandshake,

    /// Substream is being accepted (handshake received and sent in one go)
    /// as the substream was already accepted.
    // TODO: zzz
    _Accepting,

    /// Substream is open.
    Open {
        /// Inbound substream.
        inbound: Box<dyn Substream>,
    },
}

/// Outbound substream state.
#[derive(Debug)]
enum OutboundState {
    /// Substream is closed.
    Closed,

    /// Outbound substream initiated.
    OutboundInitiated {
        /// Substream ID.
        substream: SubstreamId,
    },

    /// Substream is in the state of being negotiated.
    ///
    /// This process entails sending local node's handshake and reading back the remote node's
    /// handshake if they've accepted the substream or detecting that the substream was closed
    /// in case the substream was rejected.
    Negotiating,

    /// Substream is open.
    Open {
        /// Received handshake.
        handshake: Vec<u8>,

        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },
}

#[derive(Debug)]
enum PeerState {
    /// Peer state is poisoned due to invalid state transition.
    Poisoned,

    /// Connection to peer is closed.
    Closed {
        /// TODO:
        _pending_open: Option<SubstreamId>,
    },

    /// Outbound substream initiated.
    OutboundInitiated {
        /// Substream ID.
        substream: SubstreamId,
    },

    /// Substream is being validated.
    Validating {
        /// Protocol.
        protocol: ProtocolName,

        /// Outbound protocol state.
        outbound: OutboundState,

        /// Inbound protocol state.
        inbound: InboundState,
    },

    /// Notification stream has been opened.
    Open {
        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },
}

/// Peer context.
#[derive(Debug)]
struct PeerContext {
    /// Peer state.
    state: PeerState,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new() -> Self {
        Self {
            state: PeerState::Closed {
                _pending_open: None,
            },
        }
    }
}

#[derive(Debug)]
pub(crate) struct NotificationProtocol {
    /// Transport service.
    service: TransportService,

    /// Handshake bytes.
    handshake: Vec<u8>,

    /// TX channel passed to the protocol used for sending events.
    event_handle: NotificationEventHandle,

    /// RX channel passed to the protocol used for receiving commands.
    command_rx: Receiver<NotificationCommand>,

    /// Connected peers.
    peers: HashMap<PeerId, PeerContext>,

    /// Open substreams.
    substreams: SubstreamSet<PeerId>,

    /// Receivers.
    receivers: StreamMap<PeerId, Select<ReceiverStream<Vec<u8>>, ReceiverStream<Vec<u8>>>>,

    /// zzz
    negotiation: HandshakeService,
}

impl NotificationProtocol {
    pub(crate) fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            peers: HashMap::new(),
            handshake: config.handshake.clone(),
            event_handle: NotificationEventHandle::new(config.event_tx),
            command_rx: config.command_rx,
            substreams: SubstreamSet::new(),
            receivers: StreamMap::new(),
            negotiation: HandshakeService::new(config.handshake),
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                entry.insert(PeerContext::new());
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
    async fn on_connection_closed(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        match self.peers.remove(&peer) {
            Some(_) => {
                self.substreams.remove(&peer);
                self.receivers.remove(&peer);
                self.negotiation.remove_outbound(&peer);
                self.negotiation.remove_inbound(&peer);
                // TODO: only report if the peer is actually in a state where it concerns the user
                self.event_handle
                    .report_notification_stream_closed(peer)
                    .await;
                Ok(())
            }
            None => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?peer,
                    "state mismatch: peer doesn't exist"
                );
                debug_assert!(false);
                Err(Error::PeerDoesntExist(peer))
            }
        }
    }

    /// Local node opened a substream to remote node.
    async fn on_outbound_substream(
        &mut self,
        protocol: ProtocolName,
        peer: PeerId,
        substream_id: SubstreamId,
        outbound: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?protocol,
            ?peer,
            ?substream_id,
            "handle outbound substream",
        );

        // peer must exist since an outbound substream was received from them
        let context = self.peers.get_mut(&peer).expect("peer to exist");

        // the peer can be in two different states when an outbound substream has opened:
        //  - `PeerState::OutboundInitiated` - local node opened an outbound substream
        //  - `PeerState::Negotiating` - TODO
        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::OutboundInitiated { substream } => {
                debug_assert!(substream == substream_id);

                self.negotiation.negotiate_outbound(peer, outbound);
                context.state = PeerState::Validating {
                    protocol,
                    inbound: InboundState::Closed,
                    outbound: OutboundState::Negotiating,
                };
            }
            PeerState::Validating {
                protocol,
                inbound,
                outbound: outbound_state,
            } => {
                //
                match inbound {
                    InboundState::SendingHandshake | InboundState::Open { .. } => {
                        context.state = PeerState::Validating {
                            protocol,
                            inbound,
                            outbound: OutboundState::Negotiating,
                        };
                        self.negotiation.negotiate_outbound(peer, outbound);
                    }
                    inbound_state => match outbound_state {
                        OutboundState::OutboundInitiated { substream } => {
                            debug_assert!(substream == substream_id);

                            context.state = PeerState::Validating {
                                protocol,
                                inbound: inbound_state,
                                outbound: OutboundState::Negotiating,
                            };
                            self.negotiation.negotiate_outbound(peer, outbound);
                        }
                        inner_state => {
                            tracing::error!(
                                target: LOG_TARGET,
                                ?inbound_state,
                                ?inner_state,
                                "invalid state"
                            );
                            debug_assert!(false);
                        }
                    },
                }
            }
            _state => debug_assert!(false),
        }

        Ok(())
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        protocol: ProtocolName,
        peer: PeerId,
        inbound: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?protocol,
            "handle inbound substream"
        );

        // peer must exist since an inbound substream was received from them
        let context = self.peers.get_mut(&peer).expect("peer to exist");

        // the peer can be in two different states when an outbound substream has opened:
        //  - `PeerState::Closed` - remote node opened a substream to local node and it needs to be validated
        //  - `PeerState::` - TODO
        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::Closed { .. } => {
                self.negotiation.read_handshake(peer, inbound);

                context.state = PeerState::Validating {
                    protocol,
                    inbound: InboundState::ReadingHandshake,
                    outbound: OutboundState::Closed,
                };
            }
            PeerState::Validating {
                protocol,
                outbound,
                inbound: InboundState::Closed,
            } => {
                self.negotiation.read_handshake(peer, inbound);

                context.state = PeerState::Validating {
                    protocol,
                    outbound,
                    inbound: InboundState::ReadingHandshake,
                };
            }
            PeerState::OutboundInitiated { substream } => {
                self.negotiation.read_handshake(peer, inbound);

                context.state = PeerState::Validating {
                    protocol,
                    outbound: OutboundState::OutboundInitiated { substream },
                    inbound: InboundState::ReadingHandshake,
                };
            }
            state => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?peer,
                    ?state,
                    "invalid state for inbound substream"
                );
                debug_assert!(false);
            }
        }

        Ok(())
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

    /// Open substream to remote `peer`.
    async fn on_open_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::debug!(target: LOG_TARGET, ?peer, "no open connection to peer");

            self.event_handle
                .report_notification_stream_open_failure(peer, NotificationError::NoConnection)
                .await;
            return Err(Error::PeerDoesntExist(peer));
        };

        // protocol can only request a new outbound substream to be opened if the state is `Closed`
        if let PeerState::Closed { .. } = std::mem::replace(&mut context.state, PeerState::Poisoned)
        {
            let substream = self.service.open_substream(peer).await?;
            context.state = PeerState::OutboundInitiated { substream };
        }

        Ok(())
    }

    /// Close substream to remote `peer`.
    // TODO: add documentation
    // TODO: add tests
    async fn on_close_substream(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::debug!(target: LOG_TARGET, ?peer, "peer doesn't exist");
            return;
        };

        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::OutboundInitiated { substream } => {
                context.state = PeerState::Closed {
                    _pending_open: Some(substream),
                };
            }
            // TODO: introduce `NotifiationStreamRejected`
            PeerState::Validating {
                outbound, inbound, ..
            } => {
                match outbound {
                    OutboundState::Negotiating => {
                        // the outbound substream must exist in `negotiation` because the `OutboundState`
                        // is indicating that `HandshakeService` is reading/writing from/to the substream.
                        let mut outbound = self
                            .negotiation
                            .remove_outbound(&peer)
                            .expect("inbound substream to exist");

                        let _ = outbound.close().await;
                        context.state = PeerState::Closed {
                            _pending_open: None,
                        };
                    }
                    OutboundState::Open { mut outbound, .. } => {
                        let _ = outbound.close().await;
                        context.state = PeerState::Closed {
                            _pending_open: None,
                        };
                    }
                    state => {
                        tracing::error!(
                            target: LOG_TARGET,
                            ?peer,
                            ?state,
                            "invalid state: attempted to close outbound substream that cannot be closed",
                        );
                        debug_assert!(false);
                    }
                }

                match inbound {
                    InboundState::ReadingHandshake
                    | InboundState::SendingHandshake
                    | InboundState::_Accepting => {
                        // the inbound substream must exist in `negotiation` because the `InboundState`
                        // is indicating that `HandshakeService` is reading/writing from/to the substream.
                        let mut inbound = self
                            .negotiation
                            .remove_inbound(&peer)
                            .expect("inbound substream to exist");

                        let _ = inbound.close().await;
                        context.state = PeerState::Closed {
                            _pending_open: None,
                        };
                    }
                    InboundState::Open { mut inbound } => {
                        let _ = inbound.close().await;
                        context.state = PeerState::Closed {
                            _pending_open: None,
                        };
                    }
                    InboundState::Closed => {}
                    state => {
                        tracing::error!(
                            target: LOG_TARGET,
                            ?peer,
                            ?state,
                            "invalid state: attempted to close inbound substream that cannot be closed",
                        );
                        debug_assert!(false);
                    }
                }
            }
            PeerState::Open { mut outbound } => match self.substreams.remove(&peer) {
                Some(mut inbound) => {
                    tracing::info!(target: LOG_TARGET, "close the substream");

                    self.receivers.remove(&peer);
                    let _ = outbound.close().await;
                    let _ = inbound.close().await;

                    context.state = PeerState::Closed {
                        _pending_open: None,
                    };
                    self.event_handle
                        .report_notification_stream_closed(peer)
                        .await;
                }
                None => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?peer,
                        "invalid state: inbound substream doesn't exist"
                    );
                }
            },
            state => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?peer,
                    ?state,
                    "invalid state: attempted to close substream on closed substream",
                );
            }
        }
    }

    /// Handle validation result.
    // TODO: add documentation
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

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::debug!(target: LOG_TARGET, ?peer, "peer doesn't exist");
            return Err(Error::PeerDoesntExist(peer));
        };

        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::Validating {
                protocol,
                outbound,
                inbound: InboundState::Validating { mut inbound },
            } => match result {
                ValidationResult::Reject => {
                    let _ = inbound.close().await;
                    context.state = PeerState::Closed {
                        _pending_open: None,
                    };

                    Ok(())
                }
                ValidationResult::Accept => match outbound {
                    OutboundState::Closed => match self.service.open_substream(peer).await {
                        Ok(substream) => {
                            self.negotiation.send_handshake(peer, inbound);
                            context.state = PeerState::Validating {
                                protocol,
                                inbound: InboundState::SendingHandshake,
                                outbound: OutboundState::OutboundInitiated { substream },
                            };
                            Ok(())
                        }
                        Err(error) => {
                            let _ = inbound.close().await;
                            context.state = PeerState::Closed {
                                _pending_open: None,
                            };
                            return Err(error);
                        }
                    },
                    _ => {
                        self.negotiation.send_handshake(peer, inbound);
                        context.state = PeerState::Validating {
                            protocol,
                            inbound: InboundState::SendingHandshake,
                            outbound,
                        };

                        Ok(())
                    }
                },
            },
            _state => {
                debug_assert!(false);
                Ok(())
            }
        }
    }

    /// Handle substream event.
    async fn on_substream_event(&mut self, peer: PeerId, message: crate::Result<BytesMut>) {
        tracing::trace!(target: LOG_TARGET, ?peer, is_ok = ?message.is_ok(), "handle substream event");

        match message {
            Ok(message) => {
                self.event_handle
                    .report_notification_received(peer, message.freeze().into())
                    .await
            }
            Err(_) => {
                self.negotiation.remove_outbound(&peer);
                self.negotiation.remove_inbound(&peer);
                self.substreams.remove(&peer);
                self.receivers.remove(&peer);
                self.peers
                    .get_mut(&peer)
                    .expect("peer to exist since an event was received")
                    .state = PeerState::Closed {
                    _pending_open: None,
                };
                // TODO: if the node is still handshaking, don't return this event
                self.event_handle
                    .report_notification_stream_closed(peer)
                    .await;
            }
        }
    }

    /// Handle negotiation event.
    async fn on_negotiation_event(&mut self, peer: PeerId, event: HandshakeEvent) {
        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::error!(target: LOG_TARGET, "invalid state: notification stream opened but peer doesn't exist");
            debug_assert!(false); // TODO: is this correct?
            return;
        };

        match event {
            HandshakeEvent::OutboundNegotiated {
                peer,
                handshake,
                substream,
            } => {
                self.negotiation.remove_outbound(&peer);

                match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                    PeerState::Validating {
                        protocol,
                        outbound: OutboundState::Negotiating,
                        inbound,
                    } => {
                        context.state = PeerState::Validating {
                            protocol,
                            outbound: OutboundState::Open {
                                handshake,
                                outbound: substream,
                            },
                            inbound,
                        };
                    }
                    _event => debug_assert!(false),
                }
            }
            HandshakeEvent::OutboundNegotiationError { peer } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    state = ?context.state,
                    "failed to negotiate outbound substream"
                );

                // TODO: set state properly
                let _ = self.negotiation.remove_outbound(&peer);
                return self
                    .event_handle
                    .report_notification_stream_open_failure(peer, NotificationError::Rejected)
                    .await;
            }
            HandshakeEvent::InboundNegotiated {
                peer,
                handshake,
                substream,
            } => {
                self.negotiation.remove_inbound(&peer);

                match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                    PeerState::Validating {
                        protocol,
                        outbound,
                        inbound: InboundState::ReadingHandshake,
                    } => {
                        context.state = PeerState::Validating {
                            protocol: protocol.clone(),
                            inbound: InboundState::Validating { inbound: substream },
                            outbound,
                        };

                        self.event_handle
                            .report_inbound_substream(protocol, peer, handshake.into())
                            .await;
                    }
                    PeerState::Validating {
                        protocol,
                        inbound: InboundState::SendingHandshake,
                        outbound,
                    } => {
                        context.state = PeerState::Validating {
                            protocol: protocol.clone(),
                            inbound: InboundState::Open { inbound: substream },
                            outbound,
                        };
                    }
                    _state => debug_assert!(false),
                }
            }
            HandshakeEvent::InboundAccepted { peer: _, substream } => {
                match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                    PeerState::Validating {
                        protocol,
                        outbound,
                        inbound: InboundState::_Accepting,
                    } => {
                        context.state = PeerState::Validating {
                            protocol,
                            outbound,
                            inbound: InboundState::Open { inbound: substream },
                        };
                    }
                    _state => debug_assert!(false),
                }
            }
            HandshakeEvent::InboundNegotiationError { peer } => {
                // TODO: handle error
                tracing::error!(target: LOG_TARGET, ?peer, "inbound negotaition error");
                self.negotiation.remove_inbound(&peer);
            }
        }

        // TODO: clean this code
        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::Validating {
                protocol,
                outbound:
                    OutboundState::Open {
                        handshake,
                        outbound,
                    },
                inbound: InboundState::Open { inbound },
            } => {
                tracing::debug!(target: LOG_TARGET, ?peer, "notification stream opened");

                let (async_tx, async_rx) = channel(ASYNC_CHANNEL_SIZE);
                let (sync_tx, sync_rx) = channel(SYNC_CHANNEL_SIZE);
                let notif_stream =
                    select(ReceiverStream::new(async_rx), ReceiverStream::new(sync_rx));
                let sink = NotificationSink::new(peer, sync_tx, async_tx);

                context.state = PeerState::Open { outbound };

                self.substreams.insert(peer, inbound);
                self.receivers.insert(peer, notif_stream);
                self.event_handle
                    .report_notification_stream_opened(protocol, peer, handshake.into(), sink)
                    .await;
            }
            state => context.state = state,
        }
    }

    /// Handle next notification event.
    async fn next_event(&mut self) {
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
                    if let Err(error) = self.on_connection_closed(peer).await {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to disconnect peer",
                        );
                    }
                }
                Some(TransportEvent::SubstreamOpened {
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
                            .on_outbound_substream(protocol, peer, substream_id, substream)
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
                Some(TransportEvent::SubstreamOpenFailure { substream, error }) => {
                    self.on_substream_open_failure(substream, error);
                }
                Some(TransportEvent::DialFailure { .. }) => todo!(),
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
                    NotificationCommand::CloseSubstream { peer } => {
                        self.on_close_substream(peer).await;
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
                    NotificationCommand::SetHandshake { handshake } => {
                        self.negotiation.set_handshake(handshake.clone());
                        self.handshake = handshake;
                    }
                }
            },
            event = self.substreams.next(), if !self.substreams.is_empty() => {
                let (peer, event) = event.expect("`SubstreamSet` to return `Some(..)`");
                self.on_substream_event(peer, event).await;
            }
            event = self.negotiation.next(), if !self.negotiation.is_empty() => {
                let (peer, event) = event.expect("`HandshakeService` to return `Some(..)`");

                self.on_negotiation_event(peer, event).await;
            }
            event = self.receivers.next(), if !self.receivers.is_empty() => match event {
                Some((peer, notification)) => {
                    tracing::info!(target: LOG_TARGET, ?peer, "send notification to peer");

                    match self.peers.get_mut(&peer) {
                        Some(context) => match &mut context.state {
                            PeerState::Open { outbound } => {
                                // TODO: handle error
                                let _result = outbound.send(notification.into()).await;
                            }
                            state => tracing::error!(target: LOG_TARGET, ?state, "invalid state for peer"),
                        }
                        None => {} // TODO: handle error
                    }
                }
                None => {
                    tracing::info!(target: LOG_TARGET, "here");
                }
            }
        }
    }

    /// Start [`NotificationProtocol`] event loop.
    pub(crate) async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting notification event loop");

        loop {
            self.next_event().await;
        }
    }
}
