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
    codec::ProtocolCodec,
    peer_id::PeerId,
    protocol::notification::handle::{NotificationHandle, NotificationSink},
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};

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

    /// Connection to peer doesn't exist.
    NoConnection,
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
    SetHandshake {
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
