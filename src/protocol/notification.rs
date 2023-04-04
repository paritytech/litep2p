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
    error::Error,
    peer_id::PeerId,
    protocol::TransportCommand,
    transport::{Connection, Direction, TransportEvent},
    DEFAULT_CHANNEL_SIZE,
};

use futures::{stream::FuturesUnordered, AsyncReadExt, AsyncWriteExt, StreamExt};
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

/// Unique ID for a request.
pub type SubstreamId = usize;

/// Type representing a pending substream.
// TODO: this can be simplified
type PendingSubstream = Pin<
    Box<
        dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>, Direction)>>
            + Send,
    >,
>;

/// Pending inbound substream, receiving handshake.
type PendingInbound =
    Pin<Box<dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>)>> + Send>>;

/// Pending outbound substream, sending and receiving handshakes.
type PendingOutbound =
    Pin<Box<dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>)>> + Send>>;

// TODO: is there any way to reduce the number of states?
// TODO: yes it can be, I've overcomplicated the issue

/// Peer states.
enum PeerState {
    /// Local node received an inbound substream request from an unknown node.
    InboundReceived,

    /// Inbound substream has been accepted
    InboundAccepted {
        // Substream.
        inbound: Box<dyn Connection>,
    },

    /// Handshake received from remote peer, validate substream
    UnderValidation {
        // Substream.
        inbound: Box<dyn Connection>,
    },

    /// Local node has initiated an outbound substream request.
    OutboundInitiated,

    /// Local node has initiated an outbound substream request and while the
    /// outbound substream was being processed, an inbound substream was received.
    // TODO: rename
    OutboundInitiatedInboundUnderValidation {
        // Substream.
        inbound: Box<dyn Connection>,
    },

    /// Outbound substream was initiated and the inbound substream TODO:
    OutboundInitiatedInboundReceived,

    /// Outbound opened, inbound pending
    OutboundOpened,

    /// Inbound opened, outbound pending
    InboundOpened,

    /// Both sides of the substream have been opened.
    Opened,
}

/// Validation result for an inbound substream.
pub enum ValidationResult {
    /// Accept the inbound substream.
    Accept,

    /// Reject the inbound substream.
    Reject,
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
    },

    /// Substream closed.
    SubstreamClosed {
        /// Remote peer ID.
        peer: PeerId,

        /// Substream error.
        error: (),
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

struct PeerContext {}

/// Service allowing the notification protocol to interact with `Litep2p`.
pub struct NotificationService {
    /// Protocol.
    protocol: String,

    /// Handshake.
    handshake: Vec<u8>,

    /// Peers.
    peers: HashMap<PeerId, PeerState>,

    /// TX channel for sending `TransportCommand`s to `Litep2p`.
    tx: mpsc::Sender<TransportCommand>,

    /// RX channel for receiving `TransportEvent`s from `Litep2p`.
    rx: mpsc::Receiver<TransportEvent>,

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
        Self {
            protocol,
            handshake,
            tx,
            rx,
            peers: HashMap::new(),
            pending_inbound: FuturesUnordered::new(),
            pending_outbound: FuturesUnordered::new(),
        }
    }

    /// Set handshake for the notification protocol.
    pub fn set_handshake(&mut self, handshake: Vec<u8>) {
        self.handshake = handshake;
    }

    /// Report validation result for an inbound substream.
    ///
    /// If the substream is accepted, an outbound substream is opened to the remote peer.
    pub async fn report_validation_result(
        &mut self,
        peer: PeerId,
        result: ValidationResult,
    ) -> crate::Result<()> {
        match result {
            ValidationResult::Accept => {
                // match self.peers.remove(&peer) {
                // }
                // TODO: update peer state
                self.tx
                    .send(TransportCommand::OpenSubstream {
                        protocol: self.protocol.clone(),
                        peer,
                    })
                    .await
                    .map_err(From::from)
            }
            ValidationResult::Reject => {
                self.peers.remove(&peer);
                Ok(())
            }
        }
    }

    /// Open new substream to `peer` and send them `handshake` as the initial message.
    ///
    /// This function only initiates the procedure of opening a substream. The result is
    /// polled using [`NotificationService::next_event()`].
    pub async fn open_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, protocol = ?self.protocol, "open substream");

        // verify that there isn't an open substream to the peer yet and if so,
        // ask `Litep2p` to open a new substream.
        if self.peers.get(&peer).is_some() {
            return Err(Error::PeerAlreadyExists(peer));
        }

        self.tx
            .send(TransportCommand::OpenSubstream {
                protocol: self.protocol.clone(),
                peer,
            })
            .await?;

        Ok(())
    }

    fn send_inbound_handshake(&mut self, substream: Box<dyn Connection>, state: PeerState) {
        todo!();
    }

    /// Handle inbound substream.
    fn on_inbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Connection>) {
        match self.peers.get_mut(&peer) {
            None => {
                self.peers.insert(peer, PeerState::InboundReceived);
                self.pending_inbound.push(Box::pin(async move {
                    let mut handshake = vec![0u8; 512];
                    let nread = substream.read(&mut handshake).await?;
                    Ok((peer, handshake[..nread].to_vec(), substream))
                }));
            }
            Some(_context) => {
                tracing::warn!("unimplemented some for on_inbound_substream");
                todo!();
            }
        }
    }

    fn send_outbound_handshake(
        &mut self,
        peer: PeerId,
        mut substream: Box<dyn Connection>,
        state: PeerState,
    ) {
        let handshake = self.handshake.clone();

        self.peers.insert(peer, state);
        self.pending_outbound.push(Box::pin(async move {
            let mut handshake = vec![0u8; 512];

            substream.write(&handshake).await?;
            let nread = substream.read(&mut handshake).await?;
            Ok((peer, handshake[..nread].to_vec(), substream))
        }));
    }

    /// Handle outbound substream.
    fn on_outbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Connection>) {
        tracing::info!("on outbound substream");

        // TODO: remove is probably not correct, depending on what has been reported to user thus far
        // TODO: list out all states and see what make sense
        match self.peers.remove(&peer) {
            None => {
                self.send_outbound_handshake(peer, substream, PeerState::OutboundInitiated);
            }
            Some(PeerState::InboundReceived) => {
                self.send_outbound_handshake(
                    peer,
                    substream,
                    PeerState::OutboundInitiatedInboundReceived,
                );
            }
            Some(PeerState::UnderValidation { inbound }) => {
                self.send_outbound_handshake(
                    peer,
                    substream,
                    PeerState::OutboundInitiatedInboundUnderValidation { inbound },
                );
            }
            _ => {} // other states are invalid
        }
    }

    /// Validate inbound substream.
    fn validate_inbound_substream(
        &mut self,
        peer: PeerId,
        handshake: Vec<u8>,
        inbound: Box<dyn Connection>,
    ) -> Option<NotificationEvent> {
        let mut state = self.peers.get_mut(&peer)?;

        match state {
            PeerState::InboundReceived => {
                *state = PeerState::UnderValidation { inbound };
                return Some(NotificationEvent::SubstreamReceived { peer, handshake });
            }
            PeerState::OutboundInitiated => {
                *state = PeerState::OutboundInitiatedInboundUnderValidation { inbound };
                return Some(NotificationEvent::SubstreamReceived { peer, handshake });
            }
            _ => None, // any other state is invalid
        }
    }

    /// Validate outbound substream.
    fn validate_outbound_substream(
        &mut self,
        peer: PeerId,
        handshake: Vec<u8>,
        outbound: Box<dyn Connection>,
    ) -> Option<NotificationEvent> {
        match self.peers.get_mut(&peer) {
            Some(PeerState::OutboundInitiated) => {
                todo!();
            }
            Some(PeerState::OutboundInitiatedInboundUnderValidation { inbound }) => {
                todo!();
            }
            Some(PeerState::OutboundInitiatedInboundReceived) => {
                todo!()
            }
            _ => None,
        }
    }

    /// Poll next event from the stream.
    ///
    /// [`NotificationStreamn::next_event()`] must be called in order to advance the state of the protocol.
    //
    // TODO: what needs to happen:
    //  - if we initiated the opening procedure, send handshake and wait for response
    //     - if remote sends handshake back, the substream is accepted and the remote handshake is give in `SubstreamOpened`
    //     - if remote closes the substream, our open was rejected
    //
    // - if remote initiates the procedure, we get `SubstreamReceived` event which we must either accept or reject
    //   - if we accept, we send our own handshake and open an outbound substream to remote peer.
    //
    //
    //
    pub async fn next_event(&mut self) -> Option<NotificationEvent> {
        loop {
            tokio::select! {
                event = self.rx.recv() => match event? {
                    TransportEvent::SubstreamOpened(_, peer, direction, substream) => match direction {
                        Direction::Inbound => self.on_inbound_substream(peer, substream),
                        Direction::Outbound => self.on_outbound_substream(peer, substream),
                    },
                    event => tracing::debug!(target: LOG_TARGET, ?event, "ignoring `TransportEvent`"),
                },
                event = self.pending_inbound.select_next_some(), if !self.pending_inbound.is_empty() => {
                    match event {
                        Ok((peer, handshake, substream)) => {
                            if let Some(event) = self.validate_inbound_substream(peer, handshake, substream) {
                                return Some(event)
                            };
                        }
                        Err(err) => { // TODO: error has to containt the PeerId
                            // TODO: what should be done here?
                        }
                    }
                }
                event = self.pending_outbound.select_next_some(), if !self.pending_outbound.is_empty() => {
                    match event {
                        Ok((peer, handshake, substream)) => {
                            if let Some(event) = self.validate_outbound_substream(peer, handshake, substream) {
                                return Some(event)
                            };
                        }
                        Err(err) => { // TODO: error has to containt the PeerId
                            // TODO: what should be done here?
                        }
                    }
                }
            }
        }
    }
}
