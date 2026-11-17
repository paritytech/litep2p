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
/// A [`Control`] communicates with a [`ControlledConnection`] via channels. This allows
/// a [`Control`] to be cloned and shared between tasks and threads.
#[derive(Clone, Debug)]
pub struct Control {
    open_sender: mpsc::Sender<oneshot::Sender<Result<Stream>>>,
    /// Close requests bypass opens queued behind a pending open.
    close_sender: mpsc::Sender<oneshot::Sender<()>>,
}

impl Control {
    pub fn new<T>(connection: Connection<T>) -> (Self, ControlledConnection<T>) {
        Self::with_open_buffer(connection, MAX_ACK_BACKLOG)
    }

    fn with_open_buffer<T>(
        connection: Connection<T>,
        open_buffer: usize,
    ) -> (Self, ControlledConnection<T>) {
        let (open_sender, open_receiver) = mpsc::channel(open_buffer);
        let (close_sender, close_receiver) = mpsc::channel(0);

        let control = Control {
            open_sender,
            close_sender,
        };
        let connection = ControlledConnection {
            connection,
            state: State::Idle,
            open_requests: open_receiver,
            close_requests: close_receiver,
            opening: None,
            close_reply: None,
            error: None,
        };

        (control, connection)
    }

    /// Open a new stream to the remote.
    pub async fn open_stream(&mut self) -> Result<Stream> {
        let (tx, rx) = oneshot::channel();
        self.open_sender.send(tx).await?;
        rx.await?
    }

    /// Close the connection.
    pub async fn close(&mut self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        if self.close_sender.send(tx).await.is_err() {
            // The receiver is closed which means the connection is closed.
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
    open_requests: mpsc::Receiver<oneshot::Sender<Result<Stream>>>,
    close_requests: mpsc::Receiver<oneshot::Sender<()>>,
    opening: Option<oneshot::Sender<Result<Stream>>>,
    /// Answered once the connection is closed if the close was requested by a [`Control`].
    close_reply: Option<oneshot::Sender<()>>,
    /// Error returned once the connection is closed.
    error: Option<ConnectionError>,
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
            Poll::Ready(Ok(stream)) => {
                let _ = reply.send(Ok(stream));
                true
            }
            // A failed open ends the connection, so the error is reported as the connection's.
            Poll::Ready(Err(error)) => {
                let _ = reply.send(Err(ConnectionError::Closed));
                self.error = Some(error);
                self.start_closing(None);
                true
            }
            Poll::Pending => {
                self.opening = Some(reply);
                false
            }
        }
    }

    fn start_closing(&mut self, reply: Option<oneshot::Sender<()>>) {
        if let Some(opening) = self.opening.take() {
            let _ = opening.send(Err(ConnectionError::Closed));
        }
        self.open_requests.close();
        self.close_reply = reply;
        self.state = State::Draining;
    }

    fn poll_next(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Stream>>> {
        loop {
            match self.state {
                State::Idle => {
                    if let Poll::Ready(Some(reply)) = self.close_requests.poll_next_unpin(cx) {
                        self.start_closing(Some(reply));
                        continue;
                    }

                    // Retry before `poll_next_inbound`, which returns early for every inbound
                    // stream and would otherwise starve the open.
                    if self.poll_opening(cx) {
                        continue;
                    }

                    match self.connection.poll_next_inbound(cx) {
                        Poll::Ready(Some(Ok(stream))) => return Poll::Ready(Some(Ok(stream))),
                        // The connection has ended and must not be polled for inbound streams
                        // again. Answer the outstanding requests before returning the error.
                        Poll::Ready(result) => {
                            tracing::debug!(target: LOG_TARGET, ?result, "Connection ended, answering outstanding requests");

                            self.error = result.and_then(Result::err);
                            self.start_closing(None);
                            continue;
                        }
                        Poll::Pending => {}
                    }

                    // Local drops and remote RST/FIN on unacked streams free the ACK backlog
                    // inside `poll_next_inbound` without waking the pending open.
                    if self.poll_opening(cx) {
                        continue;
                    }

                    // Queued opens wait until the pending open is answered or canceled.
                    if self.opening.is_some() {
                        return Poll::Pending;
                    }

                    match self.open_requests.poll_next_unpin(cx) {
                        Poll::Ready(Some(reply)) => self.opening = Some(reply),
                        // Last `Control` sender was dropped, close the connection.
                        Poll::Ready(None) => self.state = State::Closing,
                        Poll::Pending => return Poll::Pending,
                    }
                }
                State::Draining => match self.open_requests.poll_next_unpin(cx) {
                    Poll::Ready(Some(reply)) => {
                        let _ = reply.send(Err(ConnectionError::Closed));
                    }
                    Poll::Ready(None) => self.state = State::Closing,
                    Poll::Pending => return Poll::Pending,
                },
                State::Closing => {
                    let result = futures::ready!(self.connection.poll_close(cx));
                    self.error = self.error.take().or(result.err());
                    self.close_requests.close();
                    if let Some(reply) = self.close_reply.take() {
                        let _ = reply.send(());
                    }
                    self.state = State::AnsweringCloses;
                }
                State::AnsweringCloses => {
                    // A close racing `close_requests.close()` may be counted but not yet queued.
                    while let Some(reply) = futures::ready!(self.close_requests.poll_next_unpin(cx))
                    {
                        let _ = reply.send(());
                    }
                    self.state = State::Closed;

                    return match self.error.take() {
                        None | Some(ConnectionError::Closed) => Poll::Ready(None),
                        Some(error) => Poll::Ready(Some(Err(error))),
                    };
                }
                State::Closed => return Poll::Ready(None),
            }
        }
    }
}

enum State {
    Idle,
    /// Answering queued opens, including ones racing `start_closing`, before closing.
    Draining,
    Closing,
    /// Close requests wait for `poll_close`, so their channel is only closed afterwards.
    AnsweringCloses,
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
    use futures::{AsyncReadExt, AsyncWriteExt};
    use std::time::Duration;
    use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

    type Socket = Compat<tokio::io::DuplexStream>;

    fn controlled() -> (Control, ControlledConnection<Socket>, Socket) {
        controlled_with_open_buffer(MAX_ACK_BACKLOG)
    }

    fn controlled_with_open_buffer(
        open_buffer: usize,
    ) -> (Control, ControlledConnection<Socket>, Socket) {
        let (local, remote) = tokio::io::duplex(1 << 20);
        let connection = Connection::new(local.compat(), Config::default(), Mode::Client);
        let (control, connection) = Control::with_open_buffer(connection, open_buffer);
        (control, connection, remote.compat())
    }

    async fn timeout<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(5), future).await.expect("timed out")
    }

    /// Opens streams the remote never ACKs, so further opens wait.
    async fn fill_ack_backlog(
        control: &mut Control,
        connection: &mut ControlledConnection<Socket>,
    ) -> Vec<Stream> {
        let mut outbound = Vec::new();
        for _ in 0..MAX_ACK_BACKLOG {
            match future::select(Box::pin(control.open_stream()), connection.next()).await {
                future::Either::Left((stream, _)) => outbound.push(stream.unwrap()),
                future::Either::Right(_) => panic!("connection ended"),
            }
        }
        outbound
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
        let _outbound = fill_ack_backlog(&mut control, &mut connection).await;

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
    async fn writes_on_opened_stream_wake_connection() {
        let (mut control, mut connection, mut remote) = controlled();
        tokio::spawn(async move { while connection.next().await.is_some() {} });

        // Opening the first stream wakes the connection regardless.
        let _first = timeout(control.open_stream()).await.unwrap();
        let mut stream = timeout(control.open_stream()).await.unwrap();
        stream.write_all(b"x").await.unwrap();

        // SYN data frame for stream 3 carrying one byte.
        let frame = [0, 0, 0, 1, 0, 0, 0, 3, 0, 0, 0, 1, b'x'];
        let mut received = Vec::new();
        while !received.ends_with(&frame) {
            let mut buf = [0; 64];
            let read = timeout(remote.read(&mut buf)).await.unwrap();
            received.extend_from_slice(&buf[..read]);
        }
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
    async fn close_does_not_wait_for_pending_open() {
        let (mut control, _outbound, _inbound) = saturate_ack_backlog().await;
        let open = pending_open(&control).await;
        let queued = pending_open(&control).await;

        timeout(control.close()).await.unwrap();
        assert!(timeout(open).await.unwrap().is_err());
        assert!(timeout(queued).await.unwrap().is_err());
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
    async fn racing_and_late_closes_wait_for_connection_to_close() {
        let (local, mut remote) = tokio::io::duplex(1);
        let connection = Connection::new(local.compat(), Config::default(), Mode::Client);
        let (mut control, mut connection) = Control::new(connection);
        let (mut second, mut third) = (control.clone(), control.clone());

        let mut first = Box::pin(control.close());
        let mut racing = Box::pin(second.close());
        assert!(futures::poll!(&mut first).is_pending());
        assert!(futures::poll!(&mut racing).is_pending());
        assert!(futures::poll!(connection.next()).is_pending());
        assert!(futures::poll!(&mut racing).is_pending());

        let mut late = Box::pin(third.close());
        assert!(futures::poll!(&mut late).is_pending());
        assert!(futures::poll!(connection.next()).is_pending());
        assert!(futures::poll!(&mut late).is_pending());

        tokio::spawn(async move { tokio::io::copy(&mut remote, &mut tokio::io::sink()).await });
        assert!(timeout(connection.next()).await.is_none());
        timeout(future::try_join_all([first, racing, late])).await.unwrap();
    }

    #[tokio::test]
    async fn dropping_last_control_closes_connection() {
        let (control, mut connection, _remote) = controlled();
        drop(control);
        assert!(connection.next().now_or_never().unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn racing_closes_are_all_answered() {
        for _ in 0..2000 {
            let (control, mut connection, _remote) = controlled();

            // Keep the connection alive after it ends so only draining can answer a close.
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
            let driver = tokio::spawn(async move {
                while connection.next().await.is_some() {}
                let _ = done_rx.await;
            });

            let closes = (0..8).map(|_| {
                let mut control = control.clone();
                tokio::spawn(async move { control.close().await })
            });
            for close in timeout(future::try_join_all(closes)).await.unwrap() {
                close.unwrap();
            }

            let _ = done_tx.send(());
            timeout(driver).await.unwrap();
        }
    }

    #[tokio::test]
    async fn inbound_error_closes_connection_with_live_control() {
        let (mut control, mut connection, mut remote) = controlled();
        let _outbound = fill_ack_backlog(&mut control, &mut connection).await;

        let mut opener = control.clone();
        let mut queued = Box::pin(opener.open_stream());
        assert!(futures::poll!(&mut queued).is_pending());

        remote.write_all(&[0xff; 12]).await.unwrap();
        assert!(matches!(timeout(connection.next()).await, Some(Err(_))));

        // None of them wait for the connection to be polled again.
        assert!(timeout(queued).await.is_err());
        timeout(control.open_stream()).await.unwrap_err();
        timeout(control.close()).await.unwrap();

        assert!(timeout(connection.next()).await.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn opens_racing_inbound_error_are_all_answered() {
        for _ in 0..2000 {
            // Senders park when the buffer is full, which widens the window in which an open is
            // counted but not yet queued.
            let (control, mut connection, mut remote) = controlled_with_open_buffer(0);

            // Stop polling at the first terminal item but keep the connection alive.
            let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
            let driver = tokio::spawn(async move {
                while let Some(Ok(_)) = connection.next().await {}
                let _ = done_rx.await;
            });

            let opens: Vec<_> = (0..8)
                .map(|_| {
                    let mut control = control.clone();
                    tokio::spawn(async move { control.open_stream().await })
                })
                .collect();
            remote.write_all(&[0xff; 12]).await.unwrap();
            timeout(future::try_join_all(opens)).await.unwrap();

            let _ = done_tx.send(());
            timeout(driver).await.unwrap();
        }
    }

    #[tokio::test]
    async fn remote_go_away_answers_queued_open() {
        let (mut control, mut connection, mut remote) = controlled();
        let _outbound = fill_ack_backlog(&mut control, &mut connection).await;

        let mut queued = Box::pin(control.open_stream());
        assert!(futures::poll!(&mut queued).is_pending());

        remote.write_all(&[0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).await.unwrap();
        assert!(timeout(connection.next()).await.is_none());
        assert!(timeout(queued).await.is_err());
    }

    #[tokio::test]
    async fn open_ending_connection_returns_error() {
        let (local, _remote) = tokio::io::duplex(1 << 20);
        let mut config = Config::default();
        config.set_max_num_streams(1);
        let connection = Connection::new(local.compat(), config, Mode::Client);
        let (mut control, mut connection) = Control::new(connection);

        let _stream = match future::select(Box::pin(control.open_stream()), connection.next()).await
        {
            future::Either::Left((stream, _)) => stream.unwrap(),
            future::Either::Right(_) => panic!("connection ended"),
        };

        let mut open = Box::pin(control.open_stream());
        assert!(futures::poll!(&mut open).is_pending());

        assert!(matches!(
            timeout(connection.next()).await,
            Some(Err(ConnectionError::TooManyStreams))
        ));
        assert!(matches!(timeout(open).await, Err(ConnectionError::Closed)));
        assert!(timeout(connection.next()).await.is_none());
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
