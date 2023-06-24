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
    codec::ProtocolCodec, error::Error, peer_id::PeerId, types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::collections::HashMap;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::handle";

/// Validation result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationResult {
    /// Accept the inbound substream.
    Accept,

    /// Reject the inbound substream.
    Reject,
}

/// Notification error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationError {
    /// Remote rejected the substream.
    Rejected,
}

/// Notification events.
#[derive(Debug, Clone)]
pub(crate) enum InnerNotificationEvent {
    /// Validate substream.
    ValidateSubstream {
        /// Protocol name.
        protocol: ProtocolName,

        /// Peer ID.
        peer: PeerId,

        /// Handshake.
        handshake: Vec<u8>,
    },

    /// Notification stream opened.
    NotificationStreamOpened {
        /// Protocol name.
        protocol: ProtocolName,

        /// Peer ID.
        peer: PeerId,

        /// Handshake.
        handshake: Vec<u8>,

        /// Notification sink.
        sink: NotificationSink,
    },

    /// Notification stream closed.
    NotificationStreamClosed {
        /// Peer ID.
        peer: PeerId,
    },

    /// Failed to open notification stream.
    NotificationStreamOpenFailure {
        /// Peer ID.
        peer: PeerId,

        /// Error.
        error: NotificationError,
    },

    /// Notification received.
    NotificationReceived {
        /// Peer ID.
        peer: PeerId,

        /// Notification.
        notification: Vec<u8>,
    },
}

/// Notification events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEvent {
    /// Validate substream.
    ValidateSubstream {
        /// Protocol name.
        protocol: ProtocolName,

        /// Peer ID.
        peer: PeerId,

        /// Handshake.
        handshake: Vec<u8>,
    },

    /// Notification stream opened.
    NotificationStreamOpened {
        /// Protocol name.
        protocol: ProtocolName,

        /// Peer ID.
        peer: PeerId,

        /// Handshake.
        handshake: Vec<u8>,
    },

    /// Notification stream closed.
    NotificationStreamClosed {
        /// Peer ID.
        peer: PeerId,
    },

    /// Failed to open notification stream.
    NotificationStreamOpenFailure {
        /// Peer ID.
        peer: PeerId,

        /// Error.
        error: NotificationError,
    },

    /// Notification received.
    NotificationReceived {
        /// Peer ID.
        peer: PeerId,

        /// Notification.
        notification: Vec<u8>,
    },
}

/// Notification commands sent by the [`NotificationService`] to the protocol.
pub(crate) enum NotificationCommand {
    /// Open substream to peer.
    OpenSubstream {
        /// Peer ID.
        peer: PeerId,
    },

    /// Close substream to peer.
    CloseSubstream {
        /// Peer ID.
        peer: PeerId,
    },

    /// Set handshake.
    _SetHandshake {
        /// Handshake.
        handshake: Vec<u8>,
    },

    /// Send validation result for the inbound protocol.
    SubstreamValidated {
        /// Peer ID.
        peer: PeerId,

        /// Validation result.
        result: ValidationResult,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct NotificationSink {
    sync_tx: Sender<Vec<u8>>,
    async_tx: Sender<Vec<u8>>,
}

impl NotificationSink {
    /// Create new [`NotificationSink`].
    pub(crate) fn new(sync_tx: Sender<Vec<u8>>, async_tx: Sender<Vec<u8>>) -> Self {
        Self { async_tx, sync_tx }
    }

    /// Send notification to peer synchronously.
    pub(crate) fn send_sync_notification(&mut self, notification: Vec<u8>) {
        self.sync_tx.try_send(notification).unwrap();
    }

    /// Send notification to peer asynchronously.
    pub(crate) async fn send_async_notification(&mut self, notification: Vec<u8>) {
        // TODO: fix
        self.async_tx.try_send(notification).unwrap();
    }
}

/// Handle allowing the user protocol to interact with this notification protocol.
pub struct NotificationHandle {
    /// RX channel for receiving events from the notification protocol.
    event_rx: Receiver<InnerNotificationEvent>,

    /// TX channel for sending commands to the notification protocol.
    command_tx: Sender<NotificationCommand>,

    /// Peers.
    peers: HashMap<PeerId, NotificationSink>,
}

impl NotificationHandle {
    /// Create new [`NotificationHandle`].
    pub(crate) fn new(
        event_rx: Receiver<InnerNotificationEvent>,
        command_tx: Sender<NotificationCommand>,
    ) -> Self {
        Self {
            event_rx,
            command_tx,
            peers: HashMap::new(),
        }
    }

    /// Open substream to peer.
    pub async fn open_substream(&self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        let _ = self
            .command_tx
            .send(NotificationCommand::OpenSubstream { peer })
            .await;
    }

    /// Close substream to peer.
    pub async fn close_substream(&self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        let _ = self
            .command_tx
            .send(NotificationCommand::CloseSubstream { peer })
            .await;
    }

    /// Send validation result to `NotificationProtocol` for the inbound substream.
    pub async fn send_validation_result(&self, peer: PeerId, result: ValidationResult) {
        tracing::trace!(target: LOG_TARGET, ?peer, ?result, "send validation result");

        let _ = self
            .command_tx
            .send(NotificationCommand::SubstreamValidated { peer, result })
            .await;
    }

    /// Send synchronous notification to user.
    pub fn send_sync_notification(&mut self, peer: PeerId, notification: Vec<u8>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "send sync notification");

        if let Some(sink) = self.peers.get_mut(&peer) {
            sink.send_sync_notification(notification);
        }
    }

    /// Send asynchronous notification to user.
    pub async fn send_async_notification(
        &mut self,
        peer: PeerId,
        notification: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "send async notification");

        match self.peers.get_mut(&peer) {
            Some(sink) => {
                sink.send_async_notification(notification).await;
                Ok(())
            }
            None => Err(Error::PeerDoesntExist(peer)),
        }
    }

    /// Poll next event from the protocol.
    pub async fn next_event(&mut self) -> Option<NotificationEvent> {
        match self.event_rx.recv().await? {
            InnerNotificationEvent::ValidateSubstream {
                protocol,
                peer,
                handshake,
            } => Some(NotificationEvent::ValidateSubstream {
                protocol,
                peer,
                handshake,
            }),
            InnerNotificationEvent::NotificationStreamOpened {
                protocol,
                peer,
                handshake,
                sink,
            } => {
                self.peers.insert(peer, sink);
                Some(NotificationEvent::NotificationStreamOpened {
                    protocol,
                    peer,
                    handshake,
                })
            }
            InnerNotificationEvent::NotificationStreamClosed { peer } => {
                self.peers.remove(&peer);
                Some(NotificationEvent::NotificationStreamClosed { peer })
            }
            InnerNotificationEvent::NotificationStreamOpenFailure { peer, error } => {
                Some(NotificationEvent::NotificationStreamOpenFailure { peer, error })
            }
            InnerNotificationEvent::NotificationReceived { peer, notification } => {
                Some(NotificationEvent::NotificationReceived { peer, notification })
            }
        }
    }
}

/// Notification configuration.
#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    pub(crate) protocol_name: ProtocolName,

    /// Protocol codec.
    pub(crate) codec: ProtocolCodec,

    /// Maximum notification size.
    _max_notification_size: usize,

    /// Handshake bytes.
    pub(crate) handshake: Vec<u8>,

    /// Protocol aliases.
    pub(crate) _protocol_aliases: Vec<ProtocolName>,

    /// TX channel passed to the protocol used for sending events.
    pub(crate) event_tx: Sender<InnerNotificationEvent>,

    /// RX channel passed to the protocol used for receiving commands.
    pub(crate) command_rx: Receiver<NotificationCommand>,
}

impl Config {
    /// Create new [`Config`].
    pub fn new(
        protocol_name: ProtocolName,
        _max_notification_size: usize,
        handshake: Vec<u8>,
        _protocol_aliases: Vec<ProtocolName>,
    ) -> (Self, NotificationHandle) {
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let (command_tx, command_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let handle = NotificationHandle::new(event_rx, command_tx);

        (
            Self {
                protocol_name,
                codec: ProtocolCodec::UnsignedVarint,
                _max_notification_size,
                handshake,
                _protocol_aliases,
                event_tx,
                command_rx,
            },
            handle,
        )
    }

    /// Get protocol name.
    pub(crate) fn protocol_name(&self) -> &ProtocolName {
        &self.protocol_name
    }
}
