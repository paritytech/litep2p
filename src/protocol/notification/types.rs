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

#[derive(Debug)]
pub enum NotificationEvent {}

#[async_trait::async_trait]
pub trait NotificationService {
    /// Open substream to peer.
    fn open_substream(&self, peer: usize);

    /// Poll next event from the protocol.
    async fn next_event(&mut self) -> Option<NotificationEvent>;
}

/// Notification commands sent by the [`NotificationService`] to the protocol.
pub enum NotificationCommand {
    /// Open substream to peer.
    OpenSubstream {
        /// Peer ID.
        peer: PeerId,
    },
}

///
pub struct NotificationHandle {
    event_rx: Receiver<NotificationEvent>,
    command_tx: Sender<NotificationCommand>,
}

impl NotificationHandle {
    /// Create new [`NotificationHandle`].
    pub fn new(
        event_rx: Receiver<NotificationEvent>,
        command_tx: Sender<NotificationCommand>,
    ) -> Self {
        Self {
            event_rx,
            command_tx,
        }
    }
}

#[async_trait::async_trait]
impl NotificationService for NotificationHandle {
    /// Open substream to peer.
    fn open_substream(&self, peer: usize) {
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
    max_notification_size: usize,

    /// Handshake bytes.
    handshake: Vec<u8>,

    /// Protocol aliases.
    protocol_aliases: Vec<ProtocolName>,

    /// TX channel passed to the protocol used for sending events.
    event_tx: Sender<NotificationEvent>,

    /// RX channel passed to the protocol used for receiving commands.
    command_rx: Receiver<NotificationCommand>,
}

impl Config {
    /// Create new [`Config`].
    pub fn new(
        protocol_name: ProtocolName,
        max_notification_size: usize,
        handshake: Vec<u8>,
        protocol_aliases: Vec<ProtocolName>,
    ) -> (Self, Box<dyn NotificationService>) {
        let (event_tx, event_rx) = channel(64);
        let (command_tx, command_rx) = channel(64);
        let handle = NotificationHandle::new(event_rx, command_tx);

        (
            Self {
                protocol_name,
                max_notification_size,
                handshake,
                protocol_aliases,
                event_tx,
                command_rx,
            },
            Box::new(handle),
        )
    }

    /// Get protocol name.
    pub(crate) fn protocol_name(&self) -> &ProtocolName {
        &self.protocol_name
    }
}
