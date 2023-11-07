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
    protocol::notification::handle::NotificationEventHandle, substream::Substream, PeerId,
};

use futures::StreamExt;
use tokio::sync::{
    mpsc::{Receiver, Sender},
    oneshot,
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::notification::connection";

/// Bidirectional substream pair representing a connection to a remote peer.
pub(crate) struct Connection {
    /// Remote peer ID.
    peer: PeerId,

    /// Inbound substreams for receiving notifications.
    inbound: Substream,

    /// Outbound substream for sending notifications.
    outbound: Substream,

    /// Handle for sending notification events to user.
    event_handle: NotificationEventHandle,

    /// TX channel used to notify [`NotificationProtocol`](super::NotificationProtocol)
    /// that the connection has been closed.
    conn_closed_tx: Sender<PeerId>,

    /// Receiver for asynchronously sent notifications.
    async_rx: Receiver<Vec<u8>>,

    /// Receiver for synchronously sent notifications.
    sync_rx: Receiver<Vec<u8>>,

    /// Oneshot receiver used by [`NotificationProtocol`](super::NotificationProtocol)
    /// to signal that local node wishes the close the connection.
    rx: oneshot::Receiver<()>,
}

/// Notify [`NotificationProtocol`](super::NotificationProtocol) that the connection was closed.
#[derive(Debug)]
enum NotifyProtocol {
    /// Notify the protocol handler.
    Yes,

    /// Do not notify protocol handler.
    No,
}

impl Connection {
    /// Create new [`Connection`].
    pub(crate) fn new(
        peer: PeerId,
        inbound: Substream,
        outbound: Substream,
        event_handle: NotificationEventHandle,
        conn_closed_tx: Sender<PeerId>,
        async_rx: Receiver<Vec<u8>>,
        sync_rx: Receiver<Vec<u8>>,
    ) -> (Self, oneshot::Sender<()>) {
        let (tx, rx) = oneshot::channel();

        (
            Self {
                rx,
                peer,
                sync_rx,
                async_rx,
                inbound,
                outbound,
                event_handle,
                conn_closed_tx,
            },
            tx,
        )
    }

    /// Connection closed, clean up state.
    ///
    /// If [`NotificationProtocol`](super::NotificationProtocol) was the one that initiated
    /// shut down, it's not notified of connection getting closed.
    async fn close_connection(self, notify_protocol: NotifyProtocol) {
        let _ = self.inbound.close().await;
        let _ = self.outbound.close().await;

        if std::matches!(notify_protocol, NotifyProtocol::Yes) {
            let _ = self.conn_closed_tx.send(self.peer).await;
        }

        self.event_handle.report_notification_stream_closed(self.peer).await;
    }

    /// Start [`Connection`] event loop.
    pub async fn start(mut self) {
        tracing::debug!(target: LOG_TARGET, peer = ?self.peer, "start connection event loop");

        loop {
            tokio::select! {
                _ = &mut self.rx => {
                    tracing::debug!(target: LOG_TARGET, peer = ?self.peer, "closing connection");
                    return self.close_connection(NotifyProtocol::No).await;
                },
                event = self.inbound.next() => match event {
                    None | Some(Err(_)) => {
                        tracing::trace!(target: LOG_TARGET, peer = ?self.peer, "inbound substream closed");
                        return self.close_connection(NotifyProtocol::Yes).await;
                    }
                    Some(Ok(notification)) => {
                        self.event_handle.report_notification_received(self.peer, notification.freeze().into()).await;
                    }
                },
                // outbound substream never yields any events but it's polled so that if either one of the substreams
                // is closed by remote, it can be detected
                event = self.outbound.next() => match event {
                    Some(_) => {
                        tracing::warn!(target: LOG_TARGET, peer = ?self.peer, "read data from the outbound substream");
                    }
                    None => {
                        tracing::trace!(target: LOG_TARGET, peer = ?self.peer, "inbound substream closed");
                        return self.close_connection(NotifyProtocol::Yes).await;
                    }
                },
                notification = self.async_rx.recv() => match notification {
                    Some(notification) => if let Err(_) = self.outbound.send_framed(notification.into()).await {
                        return self.close_connection(NotifyProtocol::Yes).await;
                    },
                    None => {
                        tracing::trace!(target: LOG_TARGET, peer = ?self.peer, "notification sink closed");
                        return self.close_connection(NotifyProtocol::Yes).await;
                    }
                },
                notification = self.sync_rx.recv() => match notification {
                    Some(notification) => if let Err(_) = self.outbound.send_framed(notification.into()).await {
                        return self.close_connection(NotifyProtocol::Yes).await;
                    },
                    None => {
                        tracing::trace!(target: LOG_TARGET, peer = ?self.peer, "notification sink closed");
                        return self.close_connection(NotifyProtocol::Yes).await;
                    }
                }
            }
        }
    }
}
