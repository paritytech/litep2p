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
        notification::types::{
            InnerNotificationEvent, NotificationCommand, NotificationError, NotificationSink,
            ValidationResult,
        },
        ConnectionEvent, ConnectionService, Direction,
    },
    substream::{Substream, SubstreamSet},
    types::{protocol::ProtocolName, SubstreamId},
    TransportService,
};

use bytes::BytesMut;
use futures::{
    stream::{select, Select},
    SinkExt, StreamExt,
};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::{wrappers::ReceiverStream, StreamMap};

use std::collections::{hash_map::Entry, HashMap};

#[cfg(test)]
mod tests;
pub mod types;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::protocol";

/// Default channel size for synchronous notifications.
const SYNC_CHANNEL_SIZE: usize = 2048;

/// Default channel size for asynchronous notifications.
const ASYNC_CHANNEL_SIZE: usize = 8;

/// Peer state
#[derive(Debug)]
enum PeerState {
    /// Peer state is poisoned.
    Poisoned,

    /// Notification stream is closed.
    Closed {
        /// Substream ID for a pending outbound substream.
        // TODO: documentation
        pending_open: Option<SubstreamId>,
    },

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
#[derive(Debug)]
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
            state: PeerState::Closed { pending_open: None },
        }
    }
}

#[derive(Debug)]
pub struct NotificationProtocol {
    /// Transport service.
    service: TransportService,

    /// Handshake bytes.
    handshake: Vec<u8>,

    /// TX channel passed to the protocol used for sending events.
    event_tx: Sender<InnerNotificationEvent>,

    /// RX channel passed to the protocol used for receiving commands.
    command_rx: Receiver<NotificationCommand>,

    /// Connected peers.
    peers: HashMap<PeerId, PeerContext>,

    /// Open substreams.
    substreams: SubstreamSet<Box<dyn Substream>>,

    /// Receivers.
    receivers: StreamMap<PeerId, Select<ReceiverStream<Vec<u8>>, ReceiverStream<Vec<u8>>>>,
}

impl NotificationProtocol {
    pub fn new(service: TransportService, config: types::Config) -> Self {
        Self {
            service,
            peers: HashMap::new(),
            handshake: config.handshake,
            event_tx: config.event_tx,
            command_rx: config.command_rx,
            substreams: SubstreamSet::new(),
            receivers: StreamMap::new(),
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
    async fn on_connection_closed(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        match self.peers.remove(&peer) {
            Some(_) => {
                self.substreams.remove(&peer);
                self.receivers.remove(&peer);

                self.event_tx
                    .send(InnerNotificationEvent::NotificationStreamClosed { peer })
                    .await
                    .map_err(From::from)
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
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?protocol,
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

        // TODO: don't block here
        match substream.next().await {
            Some(Ok(handshake)) => match std::mem::replace(&mut context.state, PeerState::Poisoned)
            {
                PeerState::OutboundInitiated {
                    substream_id: stored_substream_id,
                } => {
                    if substream_id != stored_substream_id {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?substream_id,
                            ?stored_substream_id,
                            "substream ID mismatch"
                        );
                        debug_assert!(false);
                    }

                    context.state = PeerState::OutboundOpen {
                        outbound: substream,
                    };

                    Ok(())
                }
                PeerState::InboundOpenOutboundInitiated {
                    substream_id: stored_substream_id,
                    inbound,
                } => {
                    if substream_id != stored_substream_id {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?substream_id,
                            ?stored_substream_id,
                            "substream ID mismatch"
                        );
                        debug_assert!(false);
                    }

                    // set state to open and inform the user protocol about the stream
                    context.state = PeerState::Open {
                        outbound: substream,
                    };

                    self.on_notification_stream_opened(peer, protocol, handshake, inbound)
                        .await
                }
                PeerState::Closed { pending_open } if pending_open == Some(substream_id) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        ?substream_id,
                        "received stale outbound substream for closed connection"
                    );
                    context.state = PeerState::Closed { pending_open: None };
                    Ok(())
                }
                state => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?peer,
                        ?state,
                        "invalid state for {peer:?}: {state:?}"
                    );
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
                let _ = self
                    .event_tx
                    .send(InnerNotificationEvent::NotificationStreamOpenFailure {
                        peer,
                        error: NotificationError::Rejected,
                    })
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
                PeerState::Closed { .. } => {
                    context.state = PeerState::InboundOpen { inbound: substream };
                    self.event_tx
                        .send(InnerNotificationEvent::ValidateSubstream {
                            protocol,
                            peer,
                            handshake: handshake.into(),
                        })
                        .await
                        .map_err(From::from)
                }
                PeerState::OutboundOpen { outbound } => {
                    // acknowledge substream by sending our handshake, set the state to open
                    // and inform the user protocol about the stream
                    substream.send(self.handshake.clone().into()).await.unwrap();
                    context.state = PeerState::Open { outbound };

                    self.on_notification_stream_opened(peer, protocol, handshake, substream)
                        .await
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

    /// Notification stream has been opened, initialize context for the stream and inform user.
    ///
    /// The peer must be in in state `PeerState::Open` as it indicates that both substreams
    /// (inbound and oubound) are open. Other states are considered invalid state machine transitions.
    async fn on_notification_stream_opened(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        handshake: BytesMut,
        inbound: Box<dyn Substream>,
    ) -> crate::Result<()> {
        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::error!(target: LOG_TARGET, "invalid state: notification stream opened but peer doesn't exist");
            debug_assert!(false);
            return Err(Error::PeerDoesntExist(peer));
        };

        match &mut context.state {
            PeerState::Open { .. } => {
                let (async_tx, async_rx) = channel(ASYNC_CHANNEL_SIZE);
                let (sync_tx, sync_rx) = channel(SYNC_CHANNEL_SIZE);
                let notif_stream =
                    select(ReceiverStream::new(async_rx), ReceiverStream::new(sync_rx));
                let sink = NotificationSink::new(sync_tx, async_tx);

                self.substreams.insert(peer, inbound);
                self.receivers.insert(peer, notif_stream);

                // ignore error as the user protocol may have exited.
                self.event_tx
                    .send(InnerNotificationEvent::NotificationStreamOpened {
                        protocol,
                        peer,
                        sink,
                        handshake: handshake.into(),
                    })
                    .await
                    .map_err(From::from)
            }
            _ => {
                tracing::error!(
                    target: LOG_TARGET,
                    "invalid state: notification stream opened but peer doesn't exist"
                );
                debug_assert!(false);
                // TODO: introduce new error
                Ok(())
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

    /// Close substream to remote `peer`.
    // TODO: add documentation
    async fn on_close_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        match self.peers.get_mut(&peer) {
            None => Err(Error::PeerDoesntExist(peer)),
            Some(context) => {
                self.receivers.remove(&peer);
                self.substreams.remove(&peer);

                match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                    PeerState::OutboundOpen {
                        outbound: mut substream,
                    }
                    | PeerState::InboundOpen {
                        inbound: mut substream,
                    }
                    | PeerState::Open {
                        outbound: mut substream,
                    } => substream.close().await.unwrap(),
                    PeerState::InboundOpenOutboundInitiated {
                        inbound: mut substream,
                        substream_id,
                    } => {
                        substream.close().await.unwrap();
                        context.state = PeerState::Closed {
                            pending_open: Some(substream_id),
                        };
                    }
                    PeerState::OutboundInitiated { substream_id } => {
                        context.state = PeerState::Closed {
                            pending_open: Some(substream_id),
                        };
                    }
                    state => {
                        tracing::error!(target: LOG_TARGET, ?state, "invalid state for peer");
                        debug_assert!(false);
                    }
                }

                Ok(())
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
            return  Err(Error::PeerDoesntExist(peer));
        };

        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::InboundOpen { mut inbound } => match result {
                ValidationResult::Reject => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "substream rejected and closed");

                    let _ = inbound.close().await;
                    context.state = PeerState::Closed { pending_open: None };

                    Ok(())
                }
                ValidationResult::Accept => async {
                    inbound.send(self.handshake.clone().into()).await?;
                    context.service.open_substream().await.map_err(From::from)
                }
                .await
                .map(|substream_id| {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        "substream accepted and outbound substream opened"
                    );

                    context.state = PeerState::InboundOpenOutboundInitiated {
                        substream_id,
                        inbound,
                    };
                })
                .map_err(|error| {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        "failed to accept substream, now closed"
                    );

                    context.state = PeerState::Closed { pending_open: None };
                    error
                }),
            },
            state => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?state,
                    "invalid state for peer, ignoring validation result"
                );
                context.state = state;
                debug_assert!(false);
                // TODO: introduce new error for invalid state transition
                Ok(())
            }
        }
    }

    /// Handle substream event.
    async fn on_substream_event(
        &mut self,
        peer: PeerId,
        message: crate::Result<BytesMut>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, is_ok = ?message.is_ok(), "handle substream event");

        // TODO: check peer state here at some point to validate handshake

        let event = match message {
            Ok(message) => InnerNotificationEvent::NotificationReceived {
                peer,
                notification: message.freeze().into(),
            },
            Err(_) => InnerNotificationEvent::NotificationStreamClosed { peer },
        };

        self.event_tx.send(event).await.map_err(From::from)
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
                        if let Err(error) = self.on_connection_closed(peer).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to disconnect peer",
                            );
                        }
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
                        NotificationCommand::CloseSubstream { peer } => {
                            if let Err(error) = self.on_close_substream(peer).await {
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
                        NotificationCommand::SetHandshake { handshake } => {
                            self.handshake = handshake;
                        }
                    }
                },
                event = self.substreams.next(), if !self.substreams.is_empty() => {
                    let (peer, event) = event.expect("`SubstreamSet` to return `Some(..)`");

                    if let Err(error) = self.on_substream_event(peer, event).await {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to handle event from substream",
                        );
                    }
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
    }
}
