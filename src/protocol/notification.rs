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

// /// Type representing a pending substream.
// type PendingSubstream = Pin<
//     Box<
//         dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>, Direction)>>
//             + Send,
//     >,
// >;
// /// Pending inbound substream, receiving handshake.
// type PendingInbound =
//     Pin<Box<dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>)>> + Send>>;
// /// Pending outbound substream, sending and receiving handshakes.
// type PendingOutbound =
//     Pin<Box<dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>)>> + Send>>;

/// Validation result for an inbound substream.
#[derive(Debug)]
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

    /// TX channel for sending `TransportCommand`s to `Litep2p`.
    tx: mpsc::Sender<TransportCommand>,

    /// RX channel for receiving `TransportEvent`s from `Litep2p`.
    rx: mpsc::Receiver<TransportEvent>,
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
        tracing::trace!(target: LOG_TARGET, protocol = ?self.protocol, ?peer, ?result, "report validation result");

        todo!();
    }

    /// Open new substream to `peer` and send them `handshake` as the initial message.
    ///
    /// This function only initiates the procedure of opening a substream. The result is
    /// polled using [`NotificationService::next_event()`].
    pub async fn open_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, protocol = ?self.protocol, ?peer, "handle inbound substream");

        Ok(())
    }

    // fn send_outbound_handshake(
    //     &mut self,
    //     peer: PeerId,
    //     mut substream: Box<dyn Connection>,
    //     state: PeerState,
    // ) {
    //     let handshake = self.handshake.clone();

    //     self.peers.insert(peer, state);
    //     self.pending_outbound.push(Box::pin(async move {
    //         let mut handshake = vec![0u8; 512];

    //         substream.write(&handshake).await?;
    //         let nread = substream.read(&mut handshake).await?;
    //         Ok((peer, handshake[..nread].to_vec(), substream))
    //     }));
    // }

    /// Handle inbound substream.
    fn on_inbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Connection>) {
        tracing::info!(target: LOG_TARGET, protocol = ?self.protocol, ?peer, "handle inbound substream");

        // TODO: do something here
    }

    /// Handle outbound substream.
    fn on_outbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Connection>) {
        tracing::info!(target: LOG_TARGET, protocol = ?self.protocol, ?peer, "handle outbound substream");

        // TODO: do something here
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
                        Direction::Inbound => self.on_inbound_substream(peer, substream),
                        Direction::Outbound => self.on_outbound_substream(peer, substream),
                    },
                    event => tracing::debug!(target: LOG_TARGET, ?event, "ignoring `TransportEvent`"),
                },
            }
        }
    }
}
