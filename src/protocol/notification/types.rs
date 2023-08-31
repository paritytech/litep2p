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
    peer_id::PeerId, protocol::notification::handle::NotificationSink,
    types::protocol::ProtocolName,
};

/// Default channel size for synchronous notifications.
pub(super) const SYNC_CHANNEL_SIZE: usize = 16;

/// Default channel size for asynchronous notifications.
pub(super) const ASYNC_CHANNEL_SIZE: usize = 8;

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

    /// Synchronous notification channel is clogged.
    ChannelClogged,
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
