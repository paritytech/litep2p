// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

use crate::yamux::{Connection, ConnectionError, Result, Stream, MAX_ACK_BACKLOG};

use futures::{
    channel::{mpsc, oneshot},
    prelude::*,
};
use std::{
    pin::Pin,
    task::{Context, Poll},
};

const LOG_TARGET: &str = "litep2p::yamux::control";

/// A Yamux [`Connection`] controller.
///
/// This presents an alternative API for using a yamux [`Connection`].
///
/// A [`Control`] communicates with a [`ControlledConnection`] via a channel. This allows
/// a [`Control`] to be cloned and shared between tasks and threads.
#[derive(Clone, Debug)]
pub struct Control {
    /// Command channel to [`ControlledConnection`].
    sender: mpsc::Sender<ControlCommand>,
}

impl Control {
    pub fn new<T>(connection: Connection<T>) -> (Self, ControlledConnection<T>) {
        let (sender, receiver) = mpsc::channel(MAX_ACK_BACKLOG);

        let control = Control { sender };
        let connection = ControlledConnection {
            connection,
            state: State::Idle,
            commands: receiver,
            opening: None,
            close_reply: None,
        };

        (control, connection)
    }

    /// Open a new stream to the remote.
    pub async fn open_stream(&mut self) -> Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.sender.send(ControlCommand::OpenStream(tx)).await?;
        rx.await?
    }

    /// Close the connection.
    pub async fn close(&mut self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.sender.send(ControlCommand::CloseConnection(tx)).await.is_err() {
            // The receiver is closed which means the connection is already closed.
            return Ok(());
        }
        // A dropped `oneshot::Sender` means the `Connection` is gone,
        // so we do not treat receive errors differently here.
        let _ = rx.await;
        Ok(())
    }
}

/// Wraps a [`Connection`] which can be controlled with a [`Control`].
pub struct ControlledConnection<T> {
    connection: Connection<T>,
    state: State,
    commands: mpsc::Receiver<ControlCommand>,
    opening: Option<oneshot::Sender<Result<Stream>>>,
    /// Answered once the connection is closed if the close was requested by a [`Control`].
    close_reply: Option<oneshot::Sender<()>>,
}

impl<T> ControlledConnection<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    /// Returns `true` if the pending open has been answered or canceled.
    fn poll_opening(&mut self, cx: &mut Context<'_>) -> bool {
        let Some(mut reply) = self.opening.take() else {
            return false;
        };

        if reply.poll_canceled(cx).is_ready() {
            return true;
        }

        match self.connection.poll_new_outbound(cx) {
            Poll::Ready(stream) => {
                let _ = reply.send(stream);
                true
            }
            Poll::Pending => {
                self.opening = Some(reply);
                false
            }
        }
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Stream>>> {
        loop {
            match self.state {
                State::Idle => {
                    // Retry before `poll_next_inbound`, which returns early for every inbound
                    // stream and would otherwise starve the open.
                    self.poll_opening(cx);

                    match self.connection.poll_next_inbound(cx) {
                        Poll::Ready(Some(Ok(stream))) => return Poll::Ready(Some(Ok(stream))),
                        // Transport layers will close the connection on the first substream
                        // error. `poll_next_inbound` must not be called again after it returns an
                        // error, so close the connection instead.
                        Poll::Ready(Some(Err(error))) => {
                            tracing::debug!(target: LOG_TARGET, ?error, "Inbound stream error, closing connection");

                            self.state = State::Draining;
                            return Poll::Ready(Some(Err(error)));
                        }
                        Poll::Ready(None) => return Poll::Ready(None),
                        Poll::Pending => {}
                    }

                    // Local drops and remote RST/FIN on unacked streams free the ACK backlog
                    // inside `poll_next_inbound` without waking the pending open.
                    if self.poll_opening(cx) {
                        continue;
                    }

                    // Queued commands wait until the pending open is answered or canceled.
                    if self.opening.is_some() {
                        return Poll::Pending;
                    }

                    match self.commands.poll_next_unpin(cx) {
                        Poll::Ready(Some(ControlCommand::OpenStream(reply))) =>
                            self.opening = Some(reply),
                        Poll::Ready(Some(ControlCommand::CloseConnection(reply))) => {
                            self.commands.close();
                            self.close_reply = Some(reply);
                            self.state = State::Draining;
                        }
                        // Last `Control` sender was dropped, close the connection.
                        Poll::Ready(None) => self.state = State::Closing,
                        Poll::Pending => return Poll::Pending,
                    }
                }
                State::Draining => match self.commands.poll_next_unpin(cx) {
                    Poll::Ready(Some(ControlCommand::OpenStream(reply))) => {
                        let _ = reply.send(Err(ConnectionError::Closed));
                    }
                    Poll::Ready(Some(ControlCommand::CloseConnection(reply))) => {
                        let _ = reply.send(());
                    }
                    Poll::Ready(None) => self.state = State::Closing,
                    Poll::Pending => return Poll::Pending,
                },
                State::Closing => {
                    let result = futures::ready!(self.connection.poll_close(cx));
                    self.state = State::Closed;
                    if let Some(reply) = self.close_reply.take() {
                        let _ = reply.send(());
                    }

                    return match result {
                        Ok(()) | Err(ConnectionError::Closed) => Poll::Ready(None),
                        Err(error) => Poll::Ready(Some(Err(error))),
                    };
                }
                State::Closed => return Poll::Ready(None),
            }
        }
    }
}

#[derive(Debug)]
enum ControlCommand {
    /// Open a new stream to the remote end.
    OpenStream(oneshot::Sender<Result<Stream>>),
    /// Close the whole connection.
    CloseConnection(oneshot::Sender<()>),
}

enum State {
    Idle,
    /// Answering queued requests before closing.
    Draining,
    Closing,
    Closed,
}

impl<T> futures::Stream for ControlledConnection<T>
where
    T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Item = Result<Stream>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yamux::{Config, Mode};
    use futures::AsyncWriteExt;
    use std::time::Duration;
    use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

    type Socket = Compat<tokio::io::DuplexStream>;

    fn controlled() -> (Control, ControlledConnection<Socket>, Socket) {
        let (local, remote) = tokio::io::duplex(1 << 20);
        let connection = Connection::new(local.compat(), Config::default(), Mode::Client);
        let (control, connection) = Control::new(connection);
        (control, connection, remote.compat())
    }

    async fn timeout<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), future).await.expect("timed out")
    }

    /// The remote never ACKs: its inbound streams are kept alive and never written to.
    /// The connection is kept alive after it ends so dropping it cannot answer requests.
    async fn saturate_ack_backlog() -> (
        Control,
        Vec<Stream>,
        tokio::sync::mpsc::UnboundedReceiver<Stream>,
    ) {
        let (mut control, mut connection, remote) = controlled();
        tokio::spawn(async move {
            while connection.next().await.is_some() {}
            future::pending::<()>().await
        });

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut remote = Connection::new(remote, Config::default(), Mode::Server);
        tokio::spawn(async move {
            while let Some(Ok(stream)) =
                futures::future::poll_fn(|cx| remote.poll_next_inbound(cx)).await
            {
                let _ = tx.send(stream);
            }
        });

        let mut outbound = Vec::new();
        for _ in 0..MAX_ACK_BACKLOG {
            let mut stream = control.open_stream().await.unwrap();
            stream.write_all(b"syn").await.unwrap();
            outbound.push(stream);
        }

        (control, outbound, rx)
    }

    /// Spawns an open and checks that it waits for an ACK.
    async fn pending_open(control: &Control) -> tokio::task::JoinHandle<Result<Stream>> {
        let mut control = control.clone();
        let open = tokio::spawn(async move { control.open_stream().await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!open.is_finished());
        open
    }

    #[tokio::test]
    async fn opens_queue_behind_pending_open() {
        let (control, _outbound, mut inbound) = saturate_ack_backlog().await;
        let first = pending_open(&control).await;
        let second = pending_open(&control).await;

        for _ in 0..2 {
            inbound.recv().await.unwrap().write_all(b"ack").await.unwrap();
        }

        assert!(timeout(first).await.unwrap().is_ok());
        assert!(timeout(second).await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn pending_open_completes_when_unacked_stream_is_dropped() {
        let (control, mut outbound, _inbound) = saturate_ack_backlog().await;
        let open = pending_open(&control).await;

        drop(outbound.pop());

        assert!(timeout(open).await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn inbound_streams_do_not_starve_pending_open() {
        let (mut control, mut connection, mut remote) = controlled();

        let mut outbound = Vec::new();
        for _ in 0..MAX_ACK_BACKLOG {
            match future::select(Box::pin(control.open_stream()), connection.next()).await {
                future::Either::Left((stream, _)) => outbound.push(stream.unwrap()),
                future::Either::Right(_) => panic!("connection ended"),
            }
        }

        let mut pending = Box::pin(control.open_stream());
        assert!(futures::poll!(&mut pending).is_pending());
        assert!(futures::poll!(connection.next()).is_pending());

        // ACK for stream 1, then SYNs for remote streams 2, 4 and 6.
        let mut frames = vec![0, 1, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0];
        for id in [2, 4, 6] {
            frames.extend([0, 1, 0, 1, 0, 0, 0, id, 0, 0, 0, 0]);
        }
        remote.write_all(&frames).await.unwrap();

        for _ in 0..3 {
            assert!(matches!(timeout(connection.next()).await, Some(Ok(_))));
        }
        assert!(matches!(futures::poll!(&mut pending), Poll::Ready(Ok(_))));
    }

    #[tokio::test]
    async fn canceled_pending_open_does_not_open_a_stream() {
        let (mut control, outbound, mut inbound) = saturate_ack_backlog().await;
        let last_id = outbound.last().unwrap().id().val();

        let open = pending_open(&control).await;
        open.abort();
        let _ = open.await;

        inbound.recv().await.unwrap().write_all(b"ack").await.unwrap();

        let stream = timeout(control.open_stream()).await.unwrap();
        assert_eq!(stream.id().val(), last_id + 2);
    }

    #[tokio::test]
    async fn close_waits_for_connection_to_close() {
        // A one-byte pipe the remote does not read yet keeps `poll_close` pending.
        let (local, mut remote) = tokio::io::duplex(1);
        let connection = Connection::new(local.compat(), Config::default(), Mode::Client);
        let (mut control, mut connection) = Control::new(connection);

        let mut close = Box::pin(control.close());
        assert!(futures::poll!(&mut close).is_pending());
        assert!(futures::poll!(connection.next()).is_pending());
        assert!(futures::poll!(&mut close).is_pending());

        tokio::spawn(async move { tokio::io::copy(&mut remote, &mut tokio::io::sink()).await });
        assert!(timeout(connection.next()).await.is_none());
        timeout(close).await.unwrap();
    }

    #[tokio::test]
    async fn dropping_last_control_closes_connection() {
        let (control, mut connection, _remote) = controlled();
        drop(control);
        assert!(connection.next().now_or_never().unwrap().is_none());
    }

    #[tokio::test]
    async fn connection_ends_after_close_error() {
        let (reader, _remote_writer) = tokio::io::duplex(1 << 20);
        let (writer, remote_reader) = tokio::io::duplex(1 << 20);

        let socket = tokio::io::join(reader, writer).compat();
        let connection = Connection::new(socket, Config::default(), Mode::Client);
        let (mut control, mut connection) = Control::new(connection);

        assert!(futures::poll!(connection.next()).is_pending());
        drop(remote_reader);

        let close = tokio::spawn(async move { control.close().await });
        assert!(matches!(timeout(connection.next()).await, Some(Err(_))));
        timeout(close).await.unwrap().unwrap();

        assert!(timeout(connection.next()).await.is_none());
    }
}
