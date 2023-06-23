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

use crate::{peer_id::PeerId, types::protocol::ProtocolName};

use tokio::sync::mpsc::{channel, Receiver, Sender};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationEvent {}

/// Notification commands sent by the [`NotificationService`] to the protocol.
pub enum NotificationCommand {
    /// Open substream to peer.
    OpenSubstream {
        /// Peer ID.
        peer: PeerId,
    },

    /// Set handshake.
    SetHandshake {
        /// Handshake.
        handshake: Vec<u8>,
    },
}

/// Handle allowing the user protocol to interact with this notification protocol.
pub struct NotificationHandle {
    /// RX channel for receiving events from the notification protocol.
    _event_rx: Receiver<NotificationEvent>,

    /// TX channel for sending commands to the notification protocol.
    _command_tx: Sender<NotificationCommand>,
}

impl NotificationHandle {
    /// Create new [`NotificationHandle`].
    pub fn new(
        _event_rx: Receiver<NotificationEvent>,
        _command_tx: Sender<NotificationCommand>,
    ) -> Self {
        Self {
            _event_rx,
            _command_tx,
        }
    }

    /// Open substream to peer.
    async fn open_substream(&self, _peer: usize) {
        todo!();
    }

    /// Poll next event from the protocol.
    async fn next_event(&mut self) -> Option<NotificationEvent> {
        todo!();
    }
}

/// Notification configuration.
#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    protocol_name: ProtocolName,

    /// Maximum notification size.
    _max_notification_size: usize,

    /// Handshake bytes.
    _handshake: Vec<u8>,

    /// Protocol aliases.
    _protocol_aliases: Vec<ProtocolName>,

    /// TX channel passed to the protocol used for sending events.
    _event_tx: Sender<NotificationEvent>,

    /// RX channel passed to the protocol used for receiving commands.
    _command_rx: Receiver<NotificationCommand>,
}

impl Config {
    /// Create new [`Config`].
    pub fn new(
        protocol_name: ProtocolName,
        _max_notification_size: usize,
        _handshake: Vec<u8>,
        _protocol_aliases: Vec<ProtocolName>,
    ) -> (Self, NotificationHandle) {
        let (_event_tx, event_rx) = channel(64);
        let (command_tx, _command_rx) = channel(64);
        let handle = NotificationHandle::new(event_rx, command_tx);

        (
            Self {
                protocol_name,
                _max_notification_size,
                _handshake,
                _protocol_aliases,
                _event_tx,
                _command_rx,
            },
            handle,
        )
    }

    /// Get protocol name.
    pub(crate) fn protocol_name(&self) -> &ProtocolName {
        &self.protocol_name
    }
}
