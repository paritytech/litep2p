// Copyright (c) 2018-2019 Parity Technologies (UK) Ltd.
//
// Licensed under the Apache License, Version 2.0 or MIT license, at your option.
//
// A copy of the Apache License, Version 2.0 is included in the software as
// LICENSE-APACHE and a copy of the MIT license is included in the software
// as LICENSE-MIT. You may also obtain a copy of the Apache License, Version 2.0
// at https://www.apache.org/licenses/LICENSE-2.0 and a copy of the MIT license
// at https://opensource.org/licenses/MIT.

// This module contains the `Connection` type and associated helpers.
// A `Connection` wraps an underlying (async) I/O resource and multiplexes
// `Stream`s over it.
//
// The overall idea is as follows: The `Connection` makes progress via calls
// to its `next_stream` method which polls several futures, one that decodes
// `Frame`s from the I/O resource, one that consumes `ControlCommand`s
// from an MPSC channel and another one that consumes `StreamCommand`s from
// yet another MPSC channel. The latter channel is shared with every `Stream`
// created and whenever a `Stream` wishes to send a `Frame` to the remote end,
// it enqueues it into this channel (waiting if the channel is full). The
// former is shared with every `Control` clone and used to open new outbound
// streams or to trigger a connection close.
//
// The `Connection` updates the `Stream` state based on incoming frames, e.g.
// it pushes incoming data to the `Stream`'s buffer or increases the sending
// credit if the remote has sent us a corresponding `Frame::<WindowUpdate>`.
// Updating a `Stream`'s state acquires a `Mutex`, which every `Stream` has
// around its `Shared` state. While blocking, we make sure the lock is only
// held for brief moments and *never* while doing I/O. The only contention is
// between the `Connection` and a single `Stream`, which should resolve
// quickly. Ideally, we could use `futures::lock::Mutex` but it does not offer
// a poll-based API as of futures-preview 0.3.0-alpha.19, which makes it
// difficult to use in a `Stream`'s `AsyncRead` and `AsyncWrite` trait
// implementations.
//
// Closing a `Connection`
// ----------------------
//
// Every `Control` may send a `ControlCommand::Close` at any time and then
// waits on a `oneshot::Receiver` for confirmation that the connection is
// closed. The closing proceeds as follows:
//
// 1. As soon as we receive the close command we close the MPSC receiver of `StreamCommand`s. We
//    want to process any stream commands which are already enqueued at this point but no more.
// 2. We change the internal shutdown state to `Shutdown::InProgress` which contains the
//    `oneshot::Sender` of the `Control` which triggered the closure and which we need to notify
//    eventually.
// 3. Crucially -- while closing -- we no longer process further control commands, because opening
//    new streams should no longer be allowed and further close commands would mean we need to save
//    those `oneshot::Sender`s for later. On the other hand we also do not simply close the control
//    channel as this would signal to `Control`s that try to send close commands, that the
//    connection is already closed, which it is not. So we just pause processing control commands
//    which means such `Control`s will wait.
// 4. We keep processing I/O and stream commands until the remaining stream commands have all been
//    consumed, at which point we transition the shutdown state to `Shutdown::Complete`, which
//    entails sending the final termination frame to the remote, informing the `Control` and now
//    also closing the control channel.
// 5. Now that we are closed we go through all pending control commands and tell the `Control`s that
//    we are closed and we are finally done.
//
// While all of this may look complicated, it ensures that `Control`s are
// only informed about a closed connection when it really is closed.
//
// Potential improvements
// ----------------------
//
// There is always more work that can be done to make this a better crate,
// for example:
//
// - Instead of `futures::mpsc` a more efficient channel implementation could be used, e.g.
//   `tokio-sync`. Unfortunately `tokio-sync` is about to be merged into `tokio` and depending on
//   this large crate is not attractive, especially given the dire situation around cargo's flag
//   resolution.
// - Flushing could be optimised. This would also require adding a `StreamCommand::Flush` so that
//   `Stream`s can trigger a flush, which they would have to when they run out of credit, or else a
//   series of send operations might never finish.
// - If Rust gets async destructors, the `garbage_collect()` method can be removed. Instead a
//   `Stream` would send a `StreamCommand::Dropped(..)` or something similar and the removal logic
//   could happen within regular command processing instead of having to scan the whole collection
//   of `Stream`s on each loop iteration, which is not great.

mod cleanup;
mod closing;
mod stream;

use crate::yamux::{
    error::ConnectionError,
    frame::{
        self,
        header::{self, Data, GoAway, Header, Ping, StreamId, Tag, WindowUpdate, CONNECTION_ID},
        Frame,
    },
    tagged_stream::TaggedStream,
    Config, Result, WindowUpdateMode, DEFAULT_CREDIT, MAX_ACK_BACKLOG,
};
use cleanup::Cleanup;
use closing::Closing;
use futures::{
    channel::mpsc,
    future::Either,
    prelude::*,
    sink::SinkExt,
    stream::{Fuse, SelectAll},
};
use nohash_hasher::IntMap;
use parking_lot::Mutex;
use std::{
    collections::VecDeque,
    fmt,
    sync::Arc,
    task::{Context, Poll, Waker},
};

pub use stream::{Packet, State, Stream};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::yamux";

/// How the connection is used.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub enum Mode {
    /// Client to server connection.
    Client,
    /// Server to client connection.
    Server,
}

/// The connection identifier.
///
/// Randomly generated, this is mainly intended to improve log output.
#[derive(Clone, Copy)]
pub(crate) struct Id(u32);

impl Id {
    /// Create a random connection ID.
    pub(crate) fn random() -> Self {
        Id(rand::random())
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

#[derive(Debug)]
pub struct Connection<T> {
    inner: ConnectionState<T>,
}

impl<T: AsyncRead + AsyncWrite + Unpin> Connection<T> {
    pub fn new(socket: T, cfg: Config, mode: Mode) -> Self {
        Self {
            inner: ConnectionState::Active(Active::new(socket, cfg, mode)),
        }
    }

    /// Poll for a new outbound stream.
    ///
    /// This function will fail if the current state does not allow opening new outbound streams.
    pub fn poll_new_outbound(&mut self, cx: &mut Context<'_>) -> Poll<Result<Stream>> {
        loop {
            match std::mem::replace(&mut self.inner, ConnectionState::Poisoned) {
                ConnectionState::Active(mut active) => match active.poll_new_outbound(cx) {
                    Poll::Ready(Ok(stream)) => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Ready(Ok(stream));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => {
                        self.inner = ConnectionState::Cleanup(active.cleanup(e));
                        continue;
                    }
                },
                ConnectionState::Closing(mut inner) => match inner.poll_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Err(ConnectionError::Closed));
                    }
                    Poll::Ready(Err(e)) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Closing(inner);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Cleanup(mut inner) => match inner.poll_unpin(cx) {
                    Poll::Ready(e) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Err(e));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Cleanup(inner);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Closed => {
                    self.inner = ConnectionState::Closed;
                    return Poll::Ready(Err(ConnectionError::Closed));
                }
                ConnectionState::Poisoned => unreachable!(),
            }
        }
    }

    /// Poll for the next inbound stream.
    ///
    /// If this function returns `None`, the underlying connection is closed.
    pub fn poll_next_inbound(&mut self, cx: &mut Context<'_>) -> Poll<Option<Result<Stream>>> {
        loop {
            match std::mem::replace(&mut self.inner, ConnectionState::Poisoned) {
                ConnectionState::Active(mut active) => match active.poll(cx) {
                    Poll::Ready(Ok(stream)) => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Ready(Some(Ok(stream)));
                    }
                    Poll::Ready(Err(e)) => {
                        self.inner = ConnectionState::Cleanup(active.cleanup(e));
                        continue;
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Active(active);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Closing(mut closing) => match closing.poll_unpin(cx) {
                    Poll::Ready(Ok(())) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(Err(e)) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Some(Err(e)));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Closing(closing);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Cleanup(mut cleanup) => match cleanup.poll_unpin(cx) {
                    Poll::Ready(ConnectionError::Closed) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(None);
                    }
                    Poll::Ready(other) => {
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Some(Err(other)));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Cleanup(cleanup);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Closed => {
                    self.inner = ConnectionState::Closed;
                    return Poll::Ready(None);
                }
                ConnectionState::Poisoned => unreachable!(),
            }
        }
    }

    /// Close the connection.
    pub fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<Result<()>> {
        loop {
            match std::mem::replace(&mut self.inner, ConnectionState::Poisoned) {
                ConnectionState::Active(active) => {
                    self.inner = ConnectionState::Closing(active.close());
                }
                ConnectionState::Closing(mut inner) => match inner.poll_unpin(cx)? {
                    Poll::Ready(()) => {
                        self.inner = ConnectionState::Closed;
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Closing(inner);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Cleanup(mut cleanup) => match cleanup.poll_unpin(cx) {
                    Poll::Ready(reason) => {
                        tracing::warn!(target: LOG_TARGET, "Failure while closing connection: {}", reason);
                        self.inner = ConnectionState::Closed;
                        return Poll::Ready(Ok(()));
                    }
                    Poll::Pending => {
                        self.inner = ConnectionState::Cleanup(cleanup);
                        return Poll::Pending;
                    }
                },
                ConnectionState::Closed => {
                    self.inner = ConnectionState::Closed;
                    return Poll::Ready(Ok(()));
                }
                ConnectionState::Poisoned => {
                    unreachable!()
                }
            }
        }
    }
}

impl<T> Drop for Connection<T> {
    fn drop(&mut self) {
        match &mut self.inner {
            ConnectionState::Active(active) => active.drop_all_streams(),
            ConnectionState::Closing(_) => {}
            ConnectionState::Cleanup(_) => {}
            ConnectionState::Closed => {}
            ConnectionState::Poisoned => {}
        }
    }
}

enum ConnectionState<T> {
    /// The connection is alive and healthy.
    Active(Active<T>),
    /// Our user requested to shutdown the connection, we are working on it.
    Closing(Closing<T>),
    /// An error occurred and we are cleaning up our resources.
    Cleanup(Cleanup),
    /// The connection is closed.
    Closed,
    /// Something went wrong during our state transitions. Should never happen unless there is a
    /// bug.
    Poisoned,
}

impl<T> fmt::Debug for ConnectionState<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectionState::Active(_) => write!(f, "Active"),
            ConnectionState::Closing(_) => write!(f, "Closing"),
            ConnectionState::Cleanup(_) => write!(f, "Cleanup"),
            ConnectionState::Closed => write!(f, "Closed"),
            ConnectionState::Poisoned => write!(f, "Poisoned"),
        }
    }
}

/// A Yamux connection object.
///
/// Wraps the underlying I/O resource and makes progress via its
/// [`Connection::poll_next_inbound`] method which must be called repeatedly
/// until `Ok(None)` signals EOF or an error is encountered.
struct Active<T> {
    id: Id,
    mode: Mode,
    config: Arc<Config>,
    socket: Fuse<frame::Io<T>>,
    next_id: u32,

    streams: IntMap<StreamId, Arc<Mutex<stream::Shared>>>,
    stream_receivers: SelectAll<TaggedStream<StreamId, mpsc::Receiver<StreamCommand>>>,
    no_streams_waker: Option<Waker>,

    pending_frames: VecDeque<Frame<()>>,
    new_outbound_stream_waker: Option<Waker>,
}

/// `Stream` to `Connection` commands.
#[derive(Debug)]
pub(crate) enum StreamCommand {
    /// A new frame should be sent to the remote.
    SendFrame(Frame<Either<Data, WindowUpdate>>),
    /// Close a stream.
    CloseStream { ack: bool },
}

/// Possible actions as a result of incoming frame handling.
#[derive(Debug)]
enum Action {
    /// Nothing to be done.
    None,
    /// A new stream has been opened by the remote.
    New(Stream, Option<Frame<WindowUpdate>>),
    /// A window update should be sent to the remote.
    Update(Frame<WindowUpdate>),
    /// A ping should be answered.
    Ping(Frame<Ping>),
    /// A stream should be reset.
    Reset(Frame<Data>),
    /// The connection should be terminated.
    Terminate(Frame<GoAway>),
}

impl<T> fmt::Debug for Active<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Connection")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("streams", &self.streams.len())
            .field("next_id", &self.next_id)
            .finish()
    }
}

impl<T> fmt::Display for Active<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "(Connection {} {:?} (streams {}))",
            self.id,
            self.mode,
            self.streams.len()
        )
    }
}

impl<T: AsyncRead + AsyncWrite + Unpin> Active<T> {
    /// Create a new `Connection` from the given I/O resource.
    fn new(socket: T, cfg: Config, mode: Mode) -> Self {
        let id = Id::random();
        tracing::debug!(target: LOG_TARGET, "new connection: {} ({:?})", id, mode);
        let socket = frame::Io::new(id, socket, cfg.max_buffer_size).fuse();
        Active {
            id,
            mode,
            config: Arc::new(cfg),
            socket,
            streams: IntMap::default(),
            stream_receivers: SelectAll::default(),
            no_streams_waker: None,
            next_id: match mode {
                Mode::Client => 1,
                Mode::Server => 2,
            },
            pending_frames: VecDeque::default(),
            new_outbound_stream_waker: None,
        }
    }

    /// Gracefully close the connection to the remote.
    fn close(self) -> Closing<T> {
        Closing::new(self.stream_receivers, self.pending_frames, self.socket)
    }

    /// Cleanup all our resources.
    ///
    /// This should be called in the context of an unrecoverable error on the connection.
    fn cleanup(mut self, error: ConnectionError) -> Cleanup {
        self.drop_all_streams();

        Cleanup::new(self.stream_receivers, error)
    }

    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<Result<Stream>> {
        loop {
            if self.socket.poll_ready_unpin(cx).is_ready() {
                if let Some(frame) = self.pending_frames.pop_front() {
                    self.socket.start_send_unpin(frame)?;
                    continue;
                }
            }

            match self.socket.poll_flush_unpin(cx)? {
                Poll::Ready(()) => {}
                Poll::Pending => {}
            }

            match self.stream_receivers.poll_next_unpin(cx) {
                Poll::Ready(Some((_, Some(StreamCommand::SendFrame(frame))))) => {
                    self.on_send_frame(frame);
                    continue;
                }
                Poll::Ready(Some((id, Some(StreamCommand::CloseStream { ack })))) => {
                    self.on_close_stream(id, ack);
                    continue;
                }
                Poll::Ready(Some((id, None))) => {
                    self.on_drop_stream(id);
                    continue;
                }
                Poll::Ready(None) => {
                    self.no_streams_waker = Some(cx.waker().clone());
                }
                Poll::Pending => {}
            }

            match self.socket.poll_next_unpin(cx) {
                Poll::Ready(Some(frame)) => {
                    if let Some(stream) = self.on_frame(frame?)? {
                        return Poll::Ready(Ok(stream));
                    }
                    continue;
                }
                Poll::Ready(None) => {
                    return Poll::Ready(Err(ConnectionError::Closed));
                }
                Poll::Pending => {}
            }

            // If we make it this far, at least one of the above must have registered a waker.
            return Poll::Pending;
        }
    }

    fn poll_new_outbound(&mut self, cx: &mut Context<'_>) -> Poll<Result<Stream>> {
        if self.streams.len() >= self.config.max_num_streams {
            tracing::error!(target: LOG_TARGET, "{}: maximum number of streams reached", self.id);
            return Poll::Ready(Err(ConnectionError::TooManyStreams));
        }

        if self.ack_backlog() >= MAX_ACK_BACKLOG {
            tracing::debug!(target: LOG_TARGET, "{MAX_ACK_BACKLOG} streams waiting for ACK, registering task for wake-up until remote acknowledges at least one stream");
            self.new_outbound_stream_waker = Some(cx.waker().clone());
            return Poll::Pending;
        }

        tracing::trace!(target: LOG_TARGET, "{}: creating new outbound stream", self.id);

        let id = self.next_stream_id()?;
        let extra_credit = self.config.receive_window - DEFAULT_CREDIT;

        if extra_credit > 0 {
            let mut frame = Frame::window_update(id, extra_credit);
            frame.header_mut().syn();
            tracing::trace!(target: LOG_TARGET, "{}/{}: sending initial {}", self.id, id, frame.header());
            self.pending_frames.push_back(frame.into());
        }

        let mut stream = self.make_new_outbound_stream(id, self.config.receive_window);

        if extra_credit == 0 {
            stream.set_flag(stream::Flag::Syn)
        }

        tracing::debug!(target: LOG_TARGET, "{}: new outbound {} of {}", self.id, stream, self);
        self.streams.insert(id, stream.clone_shared());

        Poll::Ready(Ok(stream))
    }

    fn on_send_frame(&mut self, frame: Frame<Either<Data, WindowUpdate>>) {
        tracing::trace!(target: LOG_TARGET,
            "{}/{}: sending: {}",
            self.id,
            frame.header().stream_id(),
            frame.header()
        );
        self.pending_frames.push_back(frame.into());
    }

    fn on_close_stream(&mut self, id: StreamId, ack: bool) {
        tracing::trace!(target: LOG_TARGET, "{}/{}: sending close", self.id, id);
        self.pending_frames.push_back(Frame::close_stream(id, ack).into());
    }

    fn on_drop_stream(&mut self, stream_id: StreamId) {
        let s = self.streams.remove(&stream_id).expect("stream not found");

        tracing::trace!(target: LOG_TARGET, "{}: removing dropped stream {}", self.id, stream_id);
        let frame = {
            let mut shared = s.lock();
            let frame = match shared.update_state(self.id, stream_id, State::Closed) {
                // The stream was dropped without calling `poll_close`.
                // We reset the stream to inform the remote of the closure.
                State::Open { .. } => {
                    let mut header = Header::data(stream_id, 0);
                    header.rst();
                    Some(Frame::new(header))
                }
                // The stream was dropped without calling `poll_close`.
                // We have already received a FIN from remote and send one
                // back which closes the stream for good.
                State::RecvClosed => {
                    let mut header = Header::data(stream_id, 0);
                    header.fin();
                    Some(Frame::new(header))
                }
                // The stream was properly closed. We already sent our FIN frame.
                // The remote may be out of credit though and blocked on
                // writing more data. We may need to reset the stream.
                State::SendClosed => {
                    if self.config.window_update_mode == WindowUpdateMode::OnRead
                        && shared.window == 0
                    {
                        // The remote may be waiting for a window update
                        // which we will never send, so reset the stream now.
                        let mut header = Header::data(stream_id, 0);
                        header.rst();
                        Some(Frame::new(header))
                    } else {
                        // The remote has either still credit or will be given more
                        // (due to an enqueued window update or because the update
                        // mode is `OnReceive`) or we already have inbound frames in
                        // the socket buffer which will be processed later. In any
                        // case we will reply with an RST in `Connection::on_data`
                        // because the stream will no longer be known.
                        None
                    }
                }
                // The stream was properly closed. We already have sent our FIN frame. The
                // remote end has already done so in the past.
                State::Closed => None,
            };
            if let Some(w) = shared.reader.take() {
                w.wake()
            }
            if let Some(w) = shared.writer.take() {
                w.wake()
            }
            frame
        };
        if let Some(f) = frame {
            tracing::trace!(target: LOG_TARGET, "{}/{}: sending: {}", self.id, stream_id, f.header());
            self.pending_frames.push_back(f.into());
        }
    }

    /// Process the result of reading from the socket.
    ///
    /// Unless `frame` is `Ok(Some(_))` we will assume the connection got closed
    /// and return a corresponding error, which terminates the connection.
    /// Otherwise we process the frame and potentially return a new `Stream`
    /// if one was opened by the remote.
    fn on_frame(&mut self, frame: Frame<()>) -> Result<Option<Stream>> {
        tracing::trace!(target: LOG_TARGET, "{}: received: {}", self.id, frame.header());

        if frame.header().flags().contains(header::ACK) {
            let id = frame.header().stream_id();
            if let Some(stream) = self.streams.get(&id) {
                stream.lock().update_state(self.id, id, State::Open { acknowledged: true });
            }
            if let Some(waker) = self.new_outbound_stream_waker.take() {
                waker.wake();
            }
        }

        let action = match frame.header().tag() {
            Tag::Data => self.on_data(frame.into_data()),
            Tag::WindowUpdate => self.on_window_update(&frame.into_window_update()),
            Tag::Ping => self.on_ping(&frame.into_ping()),
            Tag::GoAway => return Err(ConnectionError::Closed),
        };
        match action {
            Action::None => {}
            Action::New(stream, update) => {
                tracing::trace!(target: LOG_TARGET, "{}: new inbound {} of {}", self.id, stream, self);
                if let Some(f) = update {
                    tracing::trace!(target: LOG_TARGET, "{}/{}: sending update", self.id, f.header().stream_id());
                    self.pending_frames.push_back(f.into());
                }
                return Ok(Some(stream));
            }
            Action::Update(f) => {
                tracing::trace!(target: LOG_TARGET, "{}: sending update: {:?}", self.id, f.header());
                self.pending_frames.push_back(f.into());
            }
            Action::Ping(f) => {
                tracing::trace!(target: LOG_TARGET, "{}/{}: pong", self.id, f.header().stream_id());
                self.pending_frames.push_back(f.into());
            }
            Action::Reset(f) => {
                tracing::trace!(target: LOG_TARGET, "{}/{}: sending reset", self.id, f.header().stream_id());
                self.pending_frames.push_back(f.into());
            }
            Action::Terminate(f) => {
                tracing::trace!(target: LOG_TARGET, "{}: sending term", self.id);
                self.pending_frames.push_back(f.into());
            }
        }

        Ok(None)
    }

    fn on_data(&mut self, frame: Frame<Data>) -> Action {
        let stream_id = frame.header().stream_id();

        if frame.header().flags().contains(header::RST) {
            // stream reset
            if let Some(s) = self.streams.get_mut(&stream_id) {
                let mut shared = s.lock();
                shared.update_state(self.id, stream_id, State::Closed);
                if let Some(w) = shared.reader.take() {
                    w.wake()
                }
                if let Some(w) = shared.writer.take() {
                    w.wake()
                }
            }
            return Action::None;
        }

        let is_finish = frame.header().flags().contains(header::FIN); // half-close

        if frame.header().flags().contains(header::SYN) {
            // new stream
            if !self.is_valid_remote_id(stream_id, Tag::Data) {
                tracing::error!(target: LOG_TARGET, "{}: invalid stream id {}", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if frame.body().len() > DEFAULT_CREDIT as usize {
                tracing::error!(target: LOG_TARGET,
                    "{}/{}: 1st body of stream exceeds default credit",
                    self.id,
                    stream_id
                );
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.contains_key(&stream_id) {
                tracing::error!(target: LOG_TARGET, "{}/{}: stream already exists", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.len() == self.config.max_num_streams {
                tracing::error!(target: LOG_TARGET, "{}: maximum number of streams reached", self.id);
                return Action::Terminate(Frame::internal_error());
            }
            let mut stream = self.make_new_inbound_stream(stream_id, DEFAULT_CREDIT);
            let mut window_update = None;
            {
                let mut shared = stream.shared();
                if is_finish {
                    shared.update_state(self.id, stream_id, State::RecvClosed);
                }
                shared.window = shared.window.saturating_sub(frame.body_len());
                shared.buffer.push(frame.into_body());

                if matches!(self.config.window_update_mode, WindowUpdateMode::OnReceive) {
                    if let Some(credit) = shared.next_window_update() {
                        shared.window += credit;
                        let mut frame = Frame::window_update(stream_id, credit);
                        frame.header_mut().ack();
                        window_update = Some(frame)
                    }
                }
            }
            if window_update.is_none() {
                stream.set_flag(stream::Flag::Ack)
            }
            self.streams.insert(stream_id, stream.clone_shared());
            return Action::New(stream, window_update);
        }

        if let Some(s) = self.streams.get_mut(&stream_id) {
            let mut shared = s.lock();
            if frame.body().len() > shared.window as usize {
                tracing::error!(target: LOG_TARGET,
                    "{}/{}: frame body larger than window of stream",
                    self.id,
                    stream_id
                );
                return Action::Terminate(Frame::protocol_error());
            }
            if is_finish {
                shared.update_state(self.id, stream_id, State::RecvClosed);
            }
            let max_buffer_size = self.config.max_buffer_size;
            if shared.buffer.len() >= max_buffer_size {
                tracing::error!(target: LOG_TARGET,
                    "{}/{}: buffer of stream grows beyond limit",
                    self.id,
                    stream_id
                );
                let mut header = Header::data(stream_id, 0);
                header.rst();
                return Action::Reset(Frame::new(header));
            }
            shared.window = shared.window.saturating_sub(frame.body_len());
            shared.buffer.push(frame.into_body());
            if let Some(w) = shared.reader.take() {
                w.wake()
            }
            if matches!(self.config.window_update_mode, WindowUpdateMode::OnReceive) {
                if let Some(credit) = shared.next_window_update() {
                    shared.window += credit;
                    let frame = Frame::window_update(stream_id, credit);
                    return Action::Update(frame);
                }
            }
        } else {
            tracing::trace!(target: LOG_TARGET,
                "{}/{}: data frame for unknown stream, possibly dropped earlier: {:?}",
                self.id,
                stream_id,
                frame
            );
            // We do not consider this a protocol violation and thus do not send a stream reset
            // because we may still be processing pending `StreamCommand`s of this stream that were
            // sent before it has been dropped and "garbage collected". Such a stream reset would
            // interfere with the frames that still need to be sent, causing premature stream
            // termination for the remote.
            //
            // See https://github.com/paritytech/yamux/issues/110 for details.
        }

        Action::None
    }

    fn on_window_update(&mut self, frame: &Frame<WindowUpdate>) -> Action {
        let stream_id = frame.header().stream_id();

        if frame.header().flags().contains(header::RST) {
            // stream reset
            if let Some(s) = self.streams.get_mut(&stream_id) {
                let mut shared = s.lock();
                shared.update_state(self.id, stream_id, State::Closed);
                if let Some(w) = shared.reader.take() {
                    w.wake()
                }
                if let Some(w) = shared.writer.take() {
                    w.wake()
                }
            }
            return Action::None;
        }

        let is_finish = frame.header().flags().contains(header::FIN); // half-close

        if frame.header().flags().contains(header::SYN) {
            // new stream
            if !self.is_valid_remote_id(stream_id, Tag::WindowUpdate) {
                tracing::error!(target: LOG_TARGET, "{}: invalid stream id {}", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.contains_key(&stream_id) {
                tracing::error!(target: LOG_TARGET, "{}/{}: stream already exists", self.id, stream_id);
                return Action::Terminate(Frame::protocol_error());
            }
            if self.streams.len() == self.config.max_num_streams {
                tracing::error!(target: LOG_TARGET, "{}: maximum number of streams reached", self.id);
                return Action::Terminate(Frame::protocol_error());
            }

            let credit = frame.header().credit() + DEFAULT_CREDIT;
            let mut stream = self.make_new_inbound_stream(stream_id, credit);
            stream.set_flag(stream::Flag::Ack);

            if is_finish {
                stream.shared().update_state(self.id, stream_id, State::RecvClosed);
            }
            self.streams.insert(stream_id, stream.clone_shared());
            return Action::New(stream, None);
        }

        if let Some(s) = self.streams.get_mut(&stream_id) {
            let mut shared = s.lock();
            shared.credit += frame.header().credit();
            if is_finish {
                shared.update_state(self.id, stream_id, State::RecvClosed);
            }
            if let Some(w) = shared.writer.take() {
                w.wake()
            }
        } else {
            tracing::trace!(target: LOG_TARGET,
                "{}/{}: window update for unknown stream, possibly dropped earlier: {:?}",
                self.id,
                stream_id,
                frame
            );
            // We do not consider this a protocol violation and thus do not send a stream reset
            // because we may still be processing pending `StreamCommand`s of this stream that were
            // sent before it has been dropped and "garbage collected". Such a stream reset would
            // interfere with the frames that still need to be sent, causing premature stream
            // termination for the remote.
            //
            // See https://github.com/paritytech/yamux/issues/110 for details.
        }

        Action::None
    }

    fn on_ping(&mut self, frame: &Frame<Ping>) -> Action {
        let stream_id = frame.header().stream_id();
        if frame.header().flags().contains(header::ACK) {
            // pong
            return Action::None;
        }
        if stream_id == CONNECTION_ID || self.streams.contains_key(&stream_id) {
            let mut hdr = Header::ping(frame.header().nonce());
            hdr.ack();
            return Action::Ping(Frame::new(hdr));
        }
        tracing::trace!(target: LOG_TARGET,
            "{}/{}: ping for unknown stream, possibly dropped earlier: {:?}",
            self.id,
            stream_id,
            frame
        );
        // We do not consider this a protocol violation and thus do not send a stream reset because
        // we may still be processing pending `StreamCommand`s of this stream that were sent before
        // it has been dropped and "garbage collected". Such a stream reset would interfere with the
        // frames that still need to be sent, causing premature stream termination for the remote.
        //
        // See https://github.com/paritytech/yamux/issues/110 for details.

        Action::None
    }

    fn make_new_inbound_stream(&mut self, id: StreamId, credit: u32) -> Stream {
        let config = self.config.clone();

        let (sender, receiver) = mpsc::channel(10); // 10 is an arbitrary number.
        self.stream_receivers.push(TaggedStream::new(id, receiver));
        if let Some(waker) = self.no_streams_waker.take() {
            waker.wake();
        }

        Stream::new_inbound(id, self.id, config, credit, sender)
    }

    fn make_new_outbound_stream(&mut self, id: StreamId, window: u32) -> Stream {
        let config = self.config.clone();

        let (sender, receiver) = mpsc::channel(10); // 10 is an arbitrary number.
        self.stream_receivers.push(TaggedStream::new(id, receiver));
        if let Some(waker) = self.no_streams_waker.take() {
            waker.wake();
        }

        Stream::new_outbound(id, self.id, config, window, sender)
    }

    fn next_stream_id(&mut self) -> Result<StreamId> {
        let proposed = StreamId::new(self.next_id);
        self.next_id = self.next_id.checked_add(2).ok_or(ConnectionError::NoMoreStreamIds)?;
        match self.mode {
            Mode::Client => assert!(proposed.is_client()),
            Mode::Server => assert!(proposed.is_server()),
        }
        Ok(proposed)
    }

    /// The ACK backlog is defined as the number of outbound streams that have not yet been
    /// acknowledged.
    fn ack_backlog(&mut self) -> usize {
        self.streams
            .iter()
            // Whether this is an outbound stream.
            //
            // Clients use odd IDs and servers use even IDs.
            // A stream is outbound if:
            //
            // - Its ID is odd and we are the client.
            // - Its ID is even and we are the server.
            .filter(|(id, _)| match self.mode {
                Mode::Client => id.is_client(),
                Mode::Server => id.is_server(),
            })
            .filter(|(_, s)| s.lock().is_pending_ack())
            .count()
    }

    // Check if the given stream ID is valid w.r.t. the provided tag and our connection mode.
    fn is_valid_remote_id(&self, id: StreamId, tag: Tag) -> bool {
        if tag == Tag::Ping || tag == Tag::GoAway {
            return id.is_session();
        }
        match self.mode {
            Mode::Client => id.is_server(),
            Mode::Server => id.is_client(),
        }
    }
}

impl<T> Active<T> {
    /// Close and drop all `Stream`s and wake any pending `Waker`s.
    fn drop_all_streams(&mut self) {
        for (id, s) in self.streams.drain() {
            let mut shared = s.lock();
            shared.update_state(self.id, id, State::Closed);
            if let Some(w) = shared.reader.take() {
                w.wake()
            }
            if let Some(w) = shared.writer.take() {
                w.wake()
            }
        }
    }
}
