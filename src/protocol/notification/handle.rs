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
    protocol::notification::types::{
        InnerNotificationEvent, NotificationCommand, NotificationError, NotificationEvent,
        ValidationResult,
    },
    types::protocol::ProtocolName,
};

use tokio::sync::mpsc::{Receiver, Sender};

use std::collections::HashMap;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::handle";

#[derive(Debug)]
pub(crate) struct NotificationEventHandle {
    tx: Sender<InnerNotificationEvent>,
}

impl NotificationEventHandle {
    /// Create new [`NotificationEventHandle`].
    pub(crate) fn new(tx: Sender<InnerNotificationEvent>) -> Self {
        Self { tx }
    }

    /// Validate inbound substream.
    pub(crate) async fn report_inbound_substream(
        &self,
        protocol: ProtocolName,
        peer: PeerId,
        handshake: Vec<u8>,
    ) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::ValidateSubstream {
                protocol,
                peer,
                handshake,
            })
            .await;
    }

    /// Notification stream opened.
    pub(crate) async fn report_notification_stream_opened(
        &self,
        protocol: ProtocolName,
        peer: PeerId,
        handshake: Vec<u8>,
        sink: NotificationSink,
    ) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::NotificationStreamOpened {
                protocol,
                peer,
                handshake,
                sink,
            })
            .await;
    }

    /// Notification stream closed.
    pub(crate) async fn report_notification_stream_closed(&self, peer: PeerId) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::NotificationStreamClosed { peer })
            .await;
    }

    /// Failed to open notification stream.
    pub(crate) async fn report_notification_stream_open_failure(
        &self,
        peer: PeerId,
        error: NotificationError,
    ) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::NotificationStreamOpenFailure { peer, error })
            .await;
    }

    /// Notification received.
    pub(crate) async fn report_notification_received(&self, peer: PeerId, notification: Vec<u8>) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::NotificationReceived { peer, notification })
            .await;
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NotificationSink {
    /// Peer ID.
    peer: PeerId,

    /// TX channel for sending notifications synchronously.
    sync_tx: Sender<Vec<u8>>,

    /// TX channel for sending notifications asynchronously.
    async_tx: Sender<Vec<u8>>,
}

impl NotificationSink {
    /// Create new [`NotificationSink`].
    pub(crate) fn new(peer: PeerId, sync_tx: Sender<Vec<u8>>, async_tx: Sender<Vec<u8>>) -> Self {
        Self {
            peer,
            async_tx,
            sync_tx,
        }
    }

    /// Send notification to peer synchronously.
    pub(crate) fn send_sync_notification(&mut self, notification: Vec<u8>) {
        self.sync_tx.try_send(notification).unwrap();
    }

    /// Send notification to peer asynchronously.
    ///
    /// Returns `Err(PeerDoesntExist(PeerId))` if the connection has been closed.
    pub(crate) async fn send_async_notification(
        &mut self,
        notification: Vec<u8>,
    ) -> crate::Result<()> {
        self.async_tx
            .send(notification)
            .await
            .map_err(|_| Error::PeerDoesntExist(self.peer))
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
    ///
    /// Returns `PeerAlreadyExists(PeerId)` if there already exists a substream to the peer.
    pub async fn open_substream(&self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        if self.peers.contains_key(&peer) {
            return Err(Error::PeerAlreadyExists(peer));
        }

        self.command_tx
            .send(NotificationCommand::OpenSubstream { peer })
            .await
            .map_or(Ok(()), |_| Ok(()))
    }

    /// Close substream to peer.
    pub async fn close_substream(&self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        if !self.peers.contains_key(&peer) {
            return;
        }

        let _ = self
            .command_tx
            .send(NotificationCommand::CloseSubstream { peer })
            .await;
    }

    /// Set new handshake.
    pub async fn set_handshake(&mut self, handshake: Vec<u8>) {
        tracing::trace!(target: LOG_TARGET, ?handshake, "set handshake");

        let _ = self
            .command_tx
            .send(NotificationCommand::SetHandshake { handshake })
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
    ///
    /// Returns `Err(PeerDoesntExist(PeerId))` if the connection has been closed.
    pub async fn send_async_notification(
        &mut self,
        peer: PeerId,
        notification: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "send async notification");

        match self.peers.get_mut(&peer) {
            Some(sink) => sink.send_async_notification(notification).await,
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
