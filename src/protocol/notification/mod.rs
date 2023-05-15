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

// TODO: return `SubstreamOpen` only when both substreams are open? woud allow simplifying the logic a bit
// TODO: fix spans to work correctly
// TODO: start using `StreamMap` for receiving inbound notifications

use crate::{
    error::{Error, NotificationError},
    peer_id::PeerId,
    protocol::TransportCommand,
    transport::{Connection, Direction, TransportEvent},
    DEFAULT_CHANNEL_SIZE,
};

use futures::{stream::FuturesUnordered, AsyncReadExt, AsyncWriteExt, Stream, StreamExt};
use tokio::sync::mpsc;

use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::Pin,
};

/// Logging target for the file.
const LOG_TARGET: &str = "notification";

/// Logging target for the file for logging binary messages.
const LOG_TARGET_MSG: &str = "notification::msg";

/// Channel size for synchronous notification sender.
const SYNC_CHANNEL_SIZE: usize = 8192;

/// Channel size for asynchronous notification sender.
const ASYNC_CHANNEL_SIZE: usize = 256;

/// Unique ID for a request.
pub type SubstreamId = usize;

/// Pending outbound substream, sending and receiving handshakes.
type PendingOutbound =
    Pin<Box<dyn Future<Output = Result<(PeerId, Vec<u8>, Box<dyn Connection>), PeerId>> + Send>>;

/// Pending inbound substream, receiving handshake.
type PendingInbound =
    Pin<Box<dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>)>> + Send>>;

/// Validation result for an inbound substream.
#[derive(Debug, PartialEq, Eq)]
pub enum ValidationResult {
    /// Accept the inbound substream.
    Accept,

    /// Reject the inbound substream.
    Reject,
}

/// Events received from inbound substream listeners
#[derive(Debug)]
pub enum InboundNotificationEvent {
    /// Inbound substream was closed.
    SubstreamClosed {
        /// Remote peer ID.
        peer: PeerId,
    },

    /// Notification received
    Notification {
        /// Remote peer ID.
        peer: PeerId,

        /// Notification.
        notification: Vec<u8>,
    },
}

#[derive(Debug)]
enum PeerState {
    /// Outbound substream initiated
    OutboundInitiated {
        /// Inbound substream.
        inbound: Option<Box<dyn Connection>>,
    },

    /// Inbound substream is being validated by the protocol.
    InboundUnderValidation {
        /// Inbound substream.
        inbound: Box<dyn Connection>,
    },

    /// Substream open to peer.
    SubstreamOpen {
        /// TX channel for sending synchronous events.
        sync_tx: mpsc::Sender<Vec<u8>>,

        /// TX channel for sending asynchronous events.
        async_tx: mpsc::Sender<Vec<u8>>,
    },
}

/// Events emitted by [`NotificationService`].
#[derive(Debug)]
pub enum NotificationEvent {
    /// Inbound substream received from remote peer.
    ///
    /// Before the substream is accepted, the protocol must check if it
    /// wants to accept this substream by doing whatever validation it needs,
    /// e.g., by checking the handshake and allocating room for it in the peer
    /// table.
    ///
    /// The acceptance result is reported using [`NotificationService::report_validation_result()`].
    // TODO: not a good name
    SubstreamReceived {
        /// Remote peer ID.
        peer: PeerId,

        /// Handshake received from the remote peer.
        handshake: Vec<u8>,
    },

    /// Substream rejected by the remote peer.
    SubstreamRejected {
        /// Remote peer ID.
        peer: PeerId,
    },

    /// Substream opened.
    SubstreamOpened {
        /// Remote peer ID.
        peer: PeerId,

        /// Handshake received from the remote peer.
        handshake: Vec<u8>,
    },

    /// Substream closed.
    SubstreamClosed {
        /// Remote peer ID.
        peer: PeerId,

        /// Substream error.
        error: (),
    },

    /// Notification received from remote peer.
    NotificationReceived {
        /// Remote peer ID.
        peer: PeerId,

        /// Received notification.
        notification: Vec<u8>,
    },
}

/// Configuration for a notification protocol.
pub struct NotificationProtocolConfig {
    /// Protocol.
    pub(crate) protocol: String,

    /// TX channel for sending `TransportEvent`s.
    pub(crate) tx: mpsc::Sender<TransportEvent>,

    /// RX channel for receiving [`TransportCommand`]s from `NotificationService`.
    pub(crate) rx: mpsc::Receiver<TransportCommand>,
}

impl NotificationProtocolConfig {
    /// Create new [`NotificationProtocolConfig`] and return [`NotificationService`]
    /// which can be used by the protocol to interact with this notification protocol.
    pub fn new(protocol: String, handshake: Vec<u8>) -> (Self, NotificationService) {
        tracing::debug!(
            target: LOG_TARGET,
            ?protocol,
            "initialize new notification protocol"
        );

        let (command_tx, command_rx) = mpsc::channel(DEFAULT_CHANNEL_SIZE);
        let (event_tx, event_rx) = mpsc::channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                protocol: protocol.clone(),
                tx: command_tx,
                rx: event_rx,
            },
            NotificationService::new(protocol, handshake, event_tx, command_rx),
        )
    }
}

/// Service allowing the notification protocol to interact with `Litep2p`.
#[derive(Debug)]
pub struct NotificationService {
    /// Protocol.
    protocol: String,

    /// Handshake.
    handshake: Vec<u8>,

    /// TX channel for sending `TransportCommand`s to `Litep2p`.
    tx: mpsc::Sender<TransportCommand>,

    /// RX channel for receiving `TransportEvent`s from `Litep2p`.
    rx: mpsc::Receiver<TransportEvent>,

    /// TX channel for sending notifications to [`NotificationService`].
    notification_tx: mpsc::Sender<InboundNotificationEvent>,

    /// RX channel for receiving notifications from peers
    notification_rx: mpsc::Receiver<InboundNotificationEvent>,

    /// Peers.
    peers: HashMap<PeerId, PeerState>,

    /// Pending inbound substreams.
    pending_inbound: FuturesUnordered<PendingInbound>,

    /// Pending outbound substreams.
    pending_outbound: FuturesUnordered<PendingOutbound>,
}

impl NotificationService {
    /// Create new [`NotificationService`].
    pub fn new(
        protocol: String,
        handshake: Vec<u8>,
        tx: mpsc::Sender<TransportCommand>,
        rx: mpsc::Receiver<TransportEvent>,
    ) -> Self {
        let (notification_tx, notification_rx) = mpsc::channel(DEFAULT_CHANNEL_SIZE); // TODO: the size has to be larger

        Self {
            protocol,
            handshake,
            tx,
            rx,
            peers: HashMap::new(),
            pending_inbound: FuturesUnordered::new(),
            pending_outbound: FuturesUnordered::new(),
            notification_tx,
            notification_rx,
        }
    }

    /// Set handshake for the notification protocol.
    pub fn set_handshake(&mut self, handshake: Vec<u8>) {
        self.handshake = handshake;
    }

    /// Send `notification` synchronously to `peer`.
    ///
    /// This function doesn't block and if the notification sink of the peer is full,
    /// the notification is silently dropped,substreams to peer are closed and this is
    /// indicated by returning `NotificationEvent::SubstreamClosed`
    pub fn send_sync_notification(
        &mut self,
        peer: PeerId,
        notification: Vec<u8>,
    ) -> crate::Result<()> {
        match self.peers.get_mut(&peer) {
            None => return Err(Error::PeerDoesntExist(peer)),
            Some(PeerState::SubstreamOpen {
                ref mut sync_tx, ..
            }) => sync_tx.try_send(notification).map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => {
                    Error::NotificationError(NotificationError::NotificationsClogged)
                }
                mpsc::error::TrySendError::Closed(_) => {
                    Error::NotificationError(NotificationError::NotificationStreamClosed(peer))
                }
            }),
            _ => return Err(Error::NotificationError(NotificationError::InvalidState)),
        }
    }

    /// Send `notification` asynchronously to `peer`.
    ///
    /// This function blocks until the notification sink has enough space for the notification.
    pub async fn send_async_notification(
        &mut self,
        peer: PeerId,
        notification: Vec<u8>,
    ) -> crate::Result<()> {
        match self.peers.get_mut(&peer) {
            None => return Err(Error::PeerDoesntExist(peer)),
            Some(PeerState::SubstreamOpen {
                ref mut async_tx, ..
            }) => async_tx.send(notification).await.map_err(|_| {
                Error::NotificationError(NotificationError::NotificationStreamClosed(peer))
            }),
            _ => return Err(Error::NotificationError(NotificationError::InvalidState)),
        }
    }

    /// Report validation result for an inbound substream.
    ///
    /// If the substream is accepted, an outbound substream is opened to the remote peer.
    pub async fn report_validation_result(
        &mut self,
        peer: PeerId,
        result: ValidationResult,
    ) -> crate::Result<()> {
        let span = tracing::span!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            "report_validation_result()"
        );
        let _lock = span.enter();
        tracing::event!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            protocol = ?self.protocol,
            ?peer,
            ?result,
            "report validation result"
        );
        let peer_state = self.peers.remove(&peer);

        // TODO: so ugly
        if result == ValidationResult::Reject {
            tracing::info!(target: LOG_TARGET, ?peer, "drop inboudn substream");
            if let Some(PeerState::InboundUnderValidation { mut inbound }) = peer_state {
                inbound.close().await.unwrap();
            }
            return Ok(());
        }

        // verify that the peer is in valid state which is only `InboundUnderValidation` and if so,
        // send them our handshake and ask `Litep2p` to open substream to them.
        let mut inbound = match peer_state {
            Some(PeerState::InboundUnderValidation { inbound }) => inbound,
            state => {
                tracing::event!(
                    target: LOG_TARGET,
                    tracing::Level::DEBUG,
                    ?state,
                    "peer is in invalid state"
                );
                return Err(Error::NotificationError(NotificationError::InvalidState));
            }
        };

        // write our handshake to remote peer, accepting the substream
        // and then ask `Litep2p` to establish substream for outbound data
        inbound.write(&self.handshake).await?;
        self.tx
            .send(TransportCommand::OpenSubstream {
                protocol: self.protocol.clone(),
                peer,
            })
            .await?;

        self.peers.insert(
            peer,
            PeerState::OutboundInitiated {
                inbound: Some(inbound),
            },
        );
        tracing::event!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            "inbound substream accepted, outbound substream established"
        );

        Ok(())
    }

    /// Open new substream to `peer` and send them `handshake` as the initial message.
    ///
    /// This function only initiates the procedure of opening a substream. The result is
    /// polled using [`NotificationService::next_event()`].
    pub async fn open_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, protocol = ?self.protocol, ?peer, "open substream");

        if self.peers.contains_key(&peer) {
            return Err(Error::PeerAlreadyExists(peer));
        }

        // set the peer state into `OutboundInitiated` to indicate that a new outbound substream
        // has been initiated and ask `Litep2p` to open a new outbound substream to peer.
        self.peers
            .insert(peer, PeerState::OutboundInitiated { inbound: None });
        self.tx
            .send(TransportCommand::OpenSubstream {
                protocol: self.protocol.clone(),
                peer,
            })
            .await
            .map_err(From::from)
    }

    /// Start event loop for receiving inbound notifications from remote peer.
    // TODO make this into an object?
    // TODO: start using streammap in the future maybe
    fn start_inbound_notification_receiver(
        &mut self,
        peer: PeerId,
        mut inbound: Box<dyn Connection>,
    ) {
        let tx = self.notification_tx.clone();
        tokio::spawn(async move {
            let mut notification = vec![0u8; 2048];
            tracing::trace!(
                target: LOG_TARGET,
                ?peer,
                "inbound notification receiver started"
            );

            loop {
                match inbound.read(&mut notification).await {
                    Ok(nread) => {
                        tracing::trace!(
                            target: LOG_TARGET_MSG,
                            ?peer,
                            notification = ?&notification[..nread],
                            "notification received"
                        );

                        let _ = tx
                            .send(InboundNotificationEvent::Notification {
                                peer,
                                notification: notification[..nread].to_vec(),
                            })
                            .await;
                    }
                    Err(err) => {
                        tracing::trace!(target: LOG_TARGET, ?peer, "inbound substream closed");
                        let _ = tx
                            .send(InboundNotificationEvent::SubstreamClosed { peer })
                            .await;
                        return;
                    }
                }
            }
        });
    }

    /// Start outbound notification sender.
    fn start_outbound_notification_sender(
        &mut self,
        peer: PeerId,
        mut outbound: Box<dyn Connection>,
    ) {
        let (sync_tx, mut sync_rx) = mpsc::channel(SYNC_CHANNEL_SIZE);
        let (async_tx, mut async_rx) = mpsc::channel(ASYNC_CHANNEL_SIZE);

        self.peers
            .insert(peer, PeerState::SubstreamOpen { sync_tx, async_tx });

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    notification = async_rx.recv() => match notification {
                        Some(notification) => if let Err(_) = outbound.write(&notification).await {
                            return
                        }
                        None => return,
                    },
                    notification = sync_rx.recv() => match notification {
                        Some(notification) => if let Err(_) = outbound.write(&notification).await {
                            return
                        }
                        None => return,
                    },
                }
            }
        });
    }

    /// Handle inbound substream.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        mut inbound: Box<dyn Connection>,
    ) -> Option<NotificationEvent> {
        tracing::span!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            "handle_inbound_substream()"
        )
        .enter();
        tracing::event!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            protocol = ?self.protocol,
            ?peer,
            "handle inbound substream"
        );

        match self.peers.get_mut(&peer) {
            None => {
                // TODO: don't read handshake here?
                let mut handshake = vec![0u8; 512];
                let nread = inbound.read(&mut handshake).await.unwrap();
                self.peers
                    .insert(peer, PeerState::InboundUnderValidation { inbound });
                Some(NotificationEvent::SubstreamReceived {
                    peer,
                    handshake: handshake[..nread].to_vec(),
                })
            }
            Some(PeerState::OutboundInitiated { inbound: None }) => {
                // accept the stream by writing our handshake
                // TODO: do this asynchronously?
                let mut handshake = vec![0u8; 512];
                let nread = inbound.read(&mut handshake).await.unwrap();
                inbound.write(&self.handshake).await.unwrap();

                tracing::event!(
                    target: LOG_TARGET,
                    tracing::Level::DEBUG,
                    "inbound substream opened successfully, waiting for outbound substream to finish"
                );

                self.peers.insert(
                    peer,
                    PeerState::OutboundInitiated {
                        inbound: Some(inbound),
                    },
                );

                None
            }
            Some(PeerState::SubstreamOpen { .. }) => {
                // TODO: don't read handshake here?
                let mut handshake = vec![0u8; 512];
                let nread = inbound.read(&mut handshake).await.unwrap();
                inbound.write(&self.handshake).await.unwrap();

                tracing::event!(
                    target: LOG_TARGET,
                    tracing::Level::DEBUG,
                    "start listening to notifications"
                );

                self.start_inbound_notification_receiver(peer, inbound);
                None
            }
            state => {
                tracing::event!(
                    target: LOG_TARGET,
                    tracing::Level::DEBUG,
                    ?state,
                    "invalid state for an inbound substream"
                );
                None
            }
        }
    }

    /// Handle outbound substream.
    fn on_outbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Connection>) {
        tracing::span!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            "handle_outbound_substream()"
        )
        .enter();
        tracing::event!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            protocol = ?self.protocol,
            ?peer,
            "handle outbound substream"
        );

        // validate peer state
        //
        // there are two valid peer states.
        // TODO: finish this comment
        match self.peers.get_mut(&peer) {
            Some(PeerState::OutboundInitiated { .. }) => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    "start handshaking with remote peer"
                );

                let handshake = self.handshake.clone();
                self.pending_outbound.push(Box::pin(async move {
                    let mut handshake = vec![0u8; 512]; // TODO: handshake size

                    substream.write(&handshake).await.map_err(|_| peer)?;

                    let nread = match substream.read(&mut handshake).await {
                        Ok(nread) => {
                            if nread == 0 {
                                return Err(peer);
                            } else {
                                nread
                            }
                        }
                        Err(err) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?err,
                                "failed to read handshake"
                            );
                            return Err(peer);
                        }
                    };

                    Ok((peer, handshake[..nread].to_vec(), substream))
                }));
            }
            state => tracing::event!(
                target: LOG_TARGET,
                tracing::Level::DEBUG,
                ?state,
                "invalid state for an outbound substream"
            ),
        }
    }

    /// Handle negotiated outbound substream.
    fn handle_negotiated_outbound_substream(
        &mut self,
        peer: PeerId,
        handshake: Vec<u8>,
        outbound: Box<dyn Connection>,
    ) -> Option<NotificationEvent> {
        tracing::span!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            "handle_negotiated_outbound_substream()"
        )
        .enter();
        tracing::event!(
            target: LOG_TARGET,
            tracing::Level::TRACE,
            protocol = ?self.protocol,
            ?peer,
            ?handshake,
            "handle negotiated outbound substream"
        );

        // TODO: explain this code
        match self.peers.remove(&peer) {
            Some(PeerState::OutboundInitiated { inbound }) => {
                if let Some(inbound) = inbound {
                    tracing::event!(
                        target: LOG_TARGET,
                        tracing::Level::TRACE,
                        "spawn event loop for receiving notifications from inbound substream"
                    );
                    self.start_inbound_notification_receiver(peer, inbound);
                }
                tracing::event!(
                    target: LOG_TARGET,
                    tracing::Level::TRACE,
                    "notification substream open to peer",
                );

                self.start_outbound_notification_sender(peer, outbound);
                return Some(NotificationEvent::SubstreamOpened { peer, handshake });
            }
            state => {
                tracing::event!(
                    target: LOG_TARGET,
                    tracing::Level::TRACE,
                    ?state,
                    "invalid state for peer",
                );
                // TODO: return substream rejected?
                todo!();
            }
        }
    }

    /// Poll next event from the stream.
    ///
    /// [`NotificationStreamn::next_event()`] must be called in order to advance the state of the protocol.
    //
    // TODO: how this should work
    //
    // - local node open a substream:
    //    - substream is opened, handshake is read and then the prepared substream is returned to user (`SubstreamOpened`)
    //    - remote opens another substream in the background and it's acknowleged silently
    //
    // - remote opens a substream:
    //    - local node receives `SubstreamReceived` event which it must either accept or reject
    //    - if accepted, substream is sent back to remote, the inbound substream is put on hold
    //    - local node opens a substream to remote peer and waits until handshake is received from remote
    //    - when handshake is received, the inbound substream put is taken out of hold
    //    - local node is sent `SubstreamOpened` event
    //
    pub async fn next_event(&mut self) -> Option<NotificationEvent> {
        loop {
            tokio::select! {
                event = self.rx.recv() => match event? {
                    TransportEvent::SubstreamOpened(_, peer, direction, substream) => match direction {
                        Direction::Inbound => if let Some(event) = self.on_inbound_substream(peer, substream).await {
                            return Some(event)
                        }
                        Direction::Outbound => self.on_outbound_substream(peer, substream),
                    },
                    // TODO: handle outbound substream open failure
                    event => tracing::debug!(target: LOG_TARGET, ?event, "ignoring `TransportEvent`"),
                },
                notification = self.notification_rx.recv() => match notification.expect("notification channel to stay open") {
                    InboundNotificationEvent::Notification { peer, notification } => {
                        return Some(NotificationEvent::NotificationReceived { peer, notification });
                    },
                    InboundNotificationEvent::SubstreamClosed { peer } => {
                        return Some(NotificationEvent::SubstreamClosed { peer, error: () });
                    }
                },
                result = self.pending_outbound.select_next_some(), if !self.pending_outbound.is_empty() => {
                    match result {
                        Ok((peer, handshake, outbound)) => {
                            if let Some(event) = self.handle_negotiated_outbound_substream(peer, handshake, outbound) {
                                return Some(event)
                            }
                        }
                        Err(peer) => {
                            tracing::debug!(target: LOG_TARGET, ?peer, "failed to negotiate outbound substream");
                            return Some(NotificationEvent::SubstreamRejected { peer });
                        }
                    }
                }
            }
        }
    }
}
