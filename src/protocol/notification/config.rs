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
    protocol::notification::{
        handle::NotificationHandle,
        types::{InnerNotificationEvent, NotificationCommand},
    },
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};

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
        max_notification_size: usize,
        handshake: Vec<u8>,
        _protocol_aliases: Vec<ProtocolName>,
    ) -> (Self, NotificationHandle) {
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let (command_tx, command_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let handle = NotificationHandle::new(event_rx, command_tx);

        (
            Self {
                protocol_name,
                codec: ProtocolCodec::UnsignedVarint(Some(max_notification_size)),
                _max_notification_size: max_notification_size,
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

/// Notification configuration builder.
pub struct ConfigBuilder {
    /// Protocol name.
    protocol_name: ProtocolName,

    /// Maximum notification size.
    max_notification_size: Option<usize>,

    /// Handshake bytes.
    handshake: Option<Vec<u8>>,
}

impl ConfigBuilder {
    /// Create new [`ConfigBuilder`].
    pub fn new(protocol_name: ProtocolName) -> Self {
        Self {
            protocol_name,
            max_notification_size: None,
            handshake: None,
        }
    }

    /// Set maximum notification size.
    pub fn with_max_size(mut self, max_notification_size: usize) -> Self {
        self.max_notification_size = Some(max_notification_size);
        self
    }

    /// Set handshake.
    pub fn with_handshake(mut self, handshake: Vec<u8>) -> Self {
        self.handshake = Some(handshake);
        self
    }

    /// Build notification configuration.
    pub fn build(mut self) -> (Config, NotificationHandle) {
        Config::new(
            self.protocol_name,
            self.max_notification_size.take().expect("notification size to be specified"),
            self.handshake.take().expect("handshake to be specified"),
            Vec::new(),
        )
    }
}
