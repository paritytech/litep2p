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
    protocol::notification::types::{
        Direction, InnerNotificationEvent, NotificationCommand, NotificationError,
        NotificationEvent, ValidationResult,
    },
    types::protocol::ProtocolName,
    PeerId,
};

use futures::Stream;
use tokio::sync::mpsc::{error::TrySendError, Receiver, Sender};

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::notification::handle";

#[derive(Debug, Clone)]
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
        fallback: Option<ProtocolName>,
        peer: PeerId,
        handshake: Vec<u8>,
    ) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::ValidateSubstream {
                protocol,
                fallback,
                peer,
                handshake,
            })
            .await;
    }

    /// Notification stream opened.
    pub(crate) async fn report_notification_stream_opened(
        &self,
        protocol: ProtocolName,
        fallback: Option<ProtocolName>,
        direction: Direction,
        peer: PeerId,
        handshake: Vec<u8>,
        sink: NotificationSink,
    ) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::NotificationStreamOpened {
                protocol,
                fallback,
                direction,
                peer,
                handshake,
                sink,
            })
            .await;
    }

    /// Notification stream closed.
    pub(crate) async fn report_notification_stream_closed(&self, peer: PeerId) {
        let _ = self.tx.send(InnerNotificationEvent::NotificationStreamClosed { peer }).await;
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

    /// Received a notification from remote peer.
    pub(crate) async fn report_notification_received(&self, peer: PeerId, notification: Vec<u8>) {
        let _ = self
            .tx
            .send(InnerNotificationEvent::NotificationReceived { peer, notification })
            .await;
    }
}

/// Notification sink.
///
/// Allows the user to send notifications both synchronously and asynchronously.
#[derive(Debug, Clone)]
pub struct NotificationSink {
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
    ///
    /// If the channel is clogged, [`NotificationError::ChannelClogged`] is returned.
    pub fn send_sync_notification(&self, notification: Vec<u8>) -> Result<(), NotificationError> {
        match self.sync_tx.try_send(notification) {
            Ok(_) => Ok(()),
            Err(error) => match error {
                TrySendError::Closed(_) => Err(NotificationError::NoConnection),
                TrySendError::Full(_) => Err(NotificationError::ChannelClogged),
            },
        }
    }

    /// Send notification to peer asynchronously.
    ///
    /// Returns [`Error::PeerDoesntExist(PeerId)`](crate::error::Error::PeerDoesntExist)
    /// if the connection has been closed.
    pub async fn send_async_notification(&self, notification: Vec<u8>) -> crate::Result<()> {
        self.async_tx
            .send(notification)
            .await
            .map_err(|_| Error::PeerDoesntExist(self.peer))
    }
}

/// Handle allowing the user protocol to interact with the notification protocol.
#[derive(Debug)]
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

    /// Open substream to `peer`.
    ///
    /// Returns [`Error::PeerAlreadyExists(PeerId)`](crate::error::Error::PeerAlreadyExists) if
    /// substream is already open to `peer`.
    pub async fn open_substream(&self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        if self.peers.contains_key(&peer) {
            return Err(Error::PeerAlreadyExists(peer));
        }

        self.command_tx
            .send(NotificationCommand::OpenSubstream {
                peers: HashSet::from_iter([peer]),
            })
            .await
            .map_or(Ok(()), |_| Ok(()))
    }

    /// Open substreams to multiple peers.
    ///
    /// Similar to [`NotificationHandle::open_substream()`] but multiple connections are initiated
    /// using a single call to `NotificationProtocol`.
    ///
    /// Peers who are already connected are ignored and returned as `Err(HashSet<PeerId>>)`.
    pub async fn open_substream_batch(
        &self,
        peers: impl Iterator<Item = PeerId>,
    ) -> Result<(), HashSet<PeerId>> {
        let (to_add, to_ignore): (Vec<_>, Vec<_>) = peers
            .map(|peer| match self.peers.contains_key(&peer) {
                true => (None, Some(peer)),
                false => (Some(peer), None),
            })
            .unzip();

        let to_add = to_add.into_iter().flatten().collect::<HashSet<_>>();
        let to_ignore = to_ignore.into_iter().flatten().collect::<HashSet<_>>();

        tracing::trace!(
            target: LOG_TARGET,
            peers_to_add = ?to_add.len(),
            peers_to_ignore = ?to_ignore.len(),
            "open substream",
        );

        let _ = self.command_tx.send(NotificationCommand::OpenSubstream { peers: to_add }).await;

        match to_ignore.is_empty() {
            true => Ok(()),
            false => Err(to_ignore),
        }
    }

    /// Close substream to `peer`.
    pub async fn close_substream(&self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        if !self.peers.contains_key(&peer) {
            return;
        }

        let _ = self
            .command_tx
            .send(NotificationCommand::CloseSubstream {
                peers: HashSet::from_iter([peer]),
            })
            .await;
    }

    /// Close substream to multiple peers.
    ///
    /// Similar to [`NotificationHandle::close_substream()`] but multiple connections are closed
    /// using a single call to `NotificationProtocol`.
    pub async fn close_substream_batch(&self, peers: impl Iterator<Item = PeerId>) {
        let peers = peers
            .filter_map(|peer| self.peers.contains_key(&peer).then_some(peer))
            .collect::<HashSet<_>>();

        if peers.is_empty() {
            return;
        }

        tracing::trace!(
            target: LOG_TARGET,
            ?peers,
            "close substreams",
        );

        let _ = self.command_tx.send(NotificationCommand::CloseSubstream { peers }).await;
    }

    /// Set new handshake.
    pub async fn set_handshake(&mut self, handshake: Vec<u8>) {
        tracing::trace!(target: LOG_TARGET, ?handshake, "set handshake");

        let _ = self.command_tx.send(NotificationCommand::SetHandshake { handshake }).await;
    }

    /// Set new handshake.
    pub fn try_set_handshake(&mut self, handshake: Vec<u8>) -> Result<(), NotificationError> {
        tracing::trace!(target: LOG_TARGET, ?handshake, "set handshake");

        match self.command_tx.try_send(NotificationCommand::SetHandshake { handshake }) {
            Err(error) => match error {
                TrySendError::Full(_) => Err(NotificationError::ChannelClogged),
                TrySendError::Closed(_) => Err(NotificationError::EssentialTaskClosed),
            },
            Ok(_) => return Ok(()),
        }
    }

    /// Send validation result to the notification protocol for the inbound substream.
    pub async fn send_validation_result(&self, peer: PeerId, result: ValidationResult) {
        tracing::trace!(target: LOG_TARGET, ?peer, ?result, "send validation result");

        let _ = self
            .command_tx
            .send(NotificationCommand::SubstreamValidated { peer, result })
            .await;
    }

    /// Send synchronous notification to `peer`.
    ///
    /// If the channel is clogged, [`NotificationError::ChannelClogged`] is returned.
    pub fn send_sync_notification(
        &mut self,
        peer: PeerId,
        notification: Vec<u8>,
    ) -> Result<(), NotificationError> {
        tracing::trace!(target: LOG_TARGET, ?peer, "send sync notification");

        match self.peers.get_mut(&peer) {
            Some(sink) => sink.send_sync_notification(notification),
            None => Ok(()),
        }
    }

    /// Send asynchronous notification to `peer`.
    ///
    /// Returns [`Error::PeerDoesntExist(PeerId)`](crate::error::Error::PeerDoesntExist) if the
    /// connection has been closed.
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

    /// Get a copy of the underlying notification sink for the peer.
    pub fn notification_sink(&self, peer: PeerId) -> Option<NotificationSink> {
        self.peers.get(&peer).and_then(|sink| Some(sink.clone()))
    }
}

impl Stream for NotificationHandle {
    type Item = NotificationEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match futures::ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(event) => match event {
                InnerNotificationEvent::NotificationStreamOpened {
                    protocol,
                    fallback,
                    direction,
                    peer,
                    handshake,
                    sink,
                } => {
                    self.peers.insert(peer, sink);

                    Poll::Ready(Some(NotificationEvent::NotificationStreamOpened {
                        protocol,
                        fallback,
                        direction,
                        peer,
                        handshake,
                    }))
                }
                InnerNotificationEvent::NotificationStreamClosed { peer } => {
                    self.peers.remove(&peer);

                    Poll::Ready(Some(NotificationEvent::NotificationStreamClosed { peer }))
                }
                event => Poll::Ready(Some(event.into())),
            },
        }
    }
}
