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

use futures::{future::Either, pin_mut};
use tokio::sync::mpsc::{error::TrySendError, Receiver, Sender};

use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::notification::handle";

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

    /// RX channel for receiving notifications from peers.
    notif_rx: Receiver<(PeerId, Vec<u8>)>,

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
        notif_rx: Receiver<(PeerId, Vec<u8>)>,
    ) -> Self {
        Self {
            event_rx,
            command_tx,
            notif_rx,
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
            .send(NotificationCommand::OpenSubstream { peer })
            .await
            .map_or(Ok(()), |_| Ok(()))
    }

    /// Close substream to `peer`.
    pub async fn close_substream(&self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        if !self.peers.contains_key(&peer) {
            return;
        }

        let _ = self.command_tx.send(NotificationCommand::CloseSubstream { peer }).await;
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

use std::future::Future;

impl futures::Stream for NotificationHandle {
    type Item = NotificationEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = Pin::into_inner(self);
        let fut = async {
            tokio::select! {
                event = this.event_rx.recv() => Either::Right(event),
                event = this.notif_rx.recv() => Either::Left(event),
            }
        };
        pin_mut!(fut);

        match fut.poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(either) => match either {
                Either::Right(event) => match event {
                    None => return Poll::Ready(None),
                    Some(event) => match event {
                        InnerNotificationEvent::NotificationStreamOpened {
                            protocol,
                            fallback,
                            direction,
                            peer,
                            handshake,
                            sink,
                        } => {
                            this.peers.insert(peer, sink);

                            Poll::Ready(Some(NotificationEvent::NotificationStreamOpened {
                                protocol,
                                fallback,
                                direction,
                                peer,
                                handshake,
                            }))
                        }
                        InnerNotificationEvent::NotificationStreamClosed { peer } => {
                            this.peers.remove(&peer);

                            Poll::Ready(Some(NotificationEvent::NotificationStreamClosed { peer }))
                        }
                        event => Poll::Ready(Some(event.into())),
                    },
                },
                Either::Left(event) => match event {
                    None => return Poll::Ready(None),
                    Some((peer, notification)) =>
                        Poll::Ready(Some(NotificationEvent::NotificationReceived {
                            peer,
                            notification,
                        })),
                },
            },
        }
    }
}
