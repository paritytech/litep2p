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

//! Implementation of the notification handshaking.

use crate::{substream::Substream, PeerId};

use futures::{FutureExt, Sink, Stream};
use futures_timer::Delay;

use std::{
    collections::{HashMap, VecDeque},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

/// Logging target for the file.
const LOG_TARGET: &str = "notification::negotiation";

/// Maximum timeout wait before for handshake before operation is considered failed.
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(10);

/// Substream direction.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Direction {
    /// Outbound substream, opened by local node.
    Outbound,

    /// Inbound substream, opened by remote node.
    Inbound,
}

/// Events emitted by [`HandshakeService`].
#[derive(Debug)]
pub enum HandshakeEvent {
    /// Outbound substream has been negotiated.
    OutboundNegotiated {
        /// Peer ID.
        peer: PeerId,

        /// Handshake.
        handshake: Vec<u8>,

        /// Substream.
        substream: Box<dyn Substream>,
    },

    /// Outbound substream has been negotiated.
    NegotiationError {
        /// Peer ID.
        peer: PeerId,

        /// Direction.
        direction: Direction,
    },

    /// Inbound substream has been negotiated.
    InboundNegotiated {
        /// Peer ID.
        peer: PeerId,

        /// Handshake.
        handshake: Vec<u8>,

        /// Substream.
        substream: Box<dyn Substream>,
    },
}

/// Outbound substream's handshake state
#[derive(Debug)]
enum HandshakeState {
    /// Send handshake to remote peer.
    SendHandshake,

    /// Sink is ready for the handshake to be sent.
    SinkReady,

    /// Handshake has been sent.
    HandshakeSent,

    /// Read handshake from remote peer.
    ReadHandshake,
}

/// Handshake service.
#[derive(Debug)]
pub(crate) struct HandshakeService {
    /// Handshake.
    handshake: Vec<u8>,

    /// Pending outbound substreams.
    /// Substreams:
    substreams: HashMap<(PeerId, Direction), (Box<dyn Substream>, Delay, HandshakeState)>,

    /// Ready substreams.
    ready: VecDeque<(PeerId, Direction, Vec<u8>)>,
}

impl HandshakeService {
    /// Create new [`HandshakeService`].
    pub fn new(handshake: Vec<u8>) -> Self {
        Self {
            handshake,
            ready: VecDeque::new(),
            substreams: HashMap::new(),
        }
    }

    /// Set handshake for the protocol.
    pub fn set_handshake(&mut self, handshake: Vec<u8>) {
        self.handshake = handshake;
    }

    /// Remove outbound substream from [`HandshakeService`].
    pub fn remove_outbound(&mut self, peer: &PeerId) -> Option<Box<dyn Substream>> {
        self.substreams
            .remove(&(*peer, Direction::Outbound))
            .map(|(substream, _, _)| substream)
    }

    /// Remove inbound substream from [`HandshakeService`].
    pub fn remove_inbound(&mut self, peer: &PeerId) -> Option<Box<dyn Substream>> {
        self.substreams
            .remove(&(*peer, Direction::Inbound))
            .map(|(substream, _, _)| substream)
    }

    /// Negotiate outbound handshake.
    pub fn negotiate_outbound(&mut self, peer: PeerId, substream: Box<dyn Substream>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "negotiate outbound");

        self.substreams.insert(
            (peer, Direction::Outbound),
            (
                substream,
                Delay::new(NEGOTIATION_TIMEOUT),
                HandshakeState::SendHandshake,
            ),
        );
    }

    /// Read handshake from remote peer.
    pub fn read_handshake(&mut self, peer: PeerId, substream: Box<dyn Substream>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "read handshake");

        self.substreams.insert(
            (peer, Direction::Inbound),
            (
                substream,
                Delay::new(NEGOTIATION_TIMEOUT),
                HandshakeState::ReadHandshake,
            ),
        );
    }

    /// Write handshake to remote peer.
    pub fn send_handshake(&mut self, peer: PeerId, substream: Box<dyn Substream>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "send handshake");

        self.substreams.insert(
            (peer, Direction::Inbound),
            (
                substream,
                Delay::new(NEGOTIATION_TIMEOUT),
                HandshakeState::SendHandshake,
            ),
        );
    }

    /// Returns `true` if [`HandshakeService`] contains no elements.
    pub fn is_empty(&self) -> bool {
        self.substreams.is_empty()
    }

    /// Pop event from the event queue.
    fn pop_event(&mut self) -> Option<(PeerId, HandshakeEvent)> {
        let (peer, direction, handshake) = self.ready.pop_front()?;

        match direction {
            Direction::Outbound => {
                let (substream, _, _) =
                    self.substreams.remove(&(peer, direction)).expect("peer to exist");

                return Some((
                    peer,
                    HandshakeEvent::OutboundNegotiated {
                        peer,
                        handshake,
                        substream,
                    },
                ));
            }
            Direction::Inbound => {
                let (substream, _, _) =
                    self.substreams.remove(&(peer, direction)).expect("peer to exist");

                return Some((
                    peer,
                    HandshakeEvent::InboundNegotiated {
                        peer,
                        handshake,
                        substream,
                    },
                ));
            }
        }
    }
}

impl Stream for HandshakeService {
    type Item = (PeerId, HandshakeEvent);

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = Pin::into_inner(self);

        if let Some(event) = inner.pop_event() {
            return Poll::Ready(Some(event));
        }

        if inner.substreams.is_empty() {
            return Poll::Pending;
        }

        'outer: for ((peer, direction), (ref mut substream, ref mut timer, state)) in
            inner.substreams.iter_mut()
        {
            if let Poll::Ready(()) = timer.poll_unpin(cx) {
                return Poll::Ready(Some((
                    *peer,
                    HandshakeEvent::NegotiationError {
                        peer: *peer,
                        direction: *direction,
                    },
                )));
            }

            loop {
                let pinned = Pin::new(&mut *substream);

                match state {
                    HandshakeState::SendHandshake => match pinned.poll_ready(cx) {
                        Poll::Ready(Ok(())) => {
                            *state = HandshakeState::SinkReady;
                            continue;
                        }
                        Poll::Ready(Err(_)) =>
                            return Poll::Ready(Some((
                                *peer,
                                HandshakeEvent::NegotiationError {
                                    peer: *peer,
                                    direction: *direction,
                                },
                            ))),
                        Poll::Pending => continue 'outer,
                    },
                    HandshakeState::SinkReady => {
                        match pinned.start_send(inner.handshake.clone().into()) {
                            Ok(()) => {
                                *state = HandshakeState::HandshakeSent;
                                continue;
                            }
                            Err(_) =>
                                return Poll::Ready(Some((
                                    *peer,
                                    HandshakeEvent::NegotiationError {
                                        peer: *peer,
                                        direction: *direction,
                                    },
                                ))),
                        }
                    }
                    HandshakeState::HandshakeSent => match pinned.poll_flush(cx) {
                        Poll::Ready(Ok(())) => match direction {
                            Direction::Outbound => {
                                *state = HandshakeState::ReadHandshake;
                                continue;
                            }
                            Direction::Inbound => {
                                inner.ready.push_back((*peer, *direction, vec![]));
                                continue 'outer;
                            }
                        },
                        Poll::Ready(Err(_)) =>
                            return Poll::Ready(Some((
                                *peer,
                                HandshakeEvent::NegotiationError {
                                    peer: *peer,
                                    direction: *direction,
                                },
                            ))),
                        Poll::Pending => continue 'outer,
                    },
                    HandshakeState::ReadHandshake => match pinned.poll_next(cx) {
                        Poll::Ready(Some(Ok(handshake))) => {
                            inner.ready.push_back((*peer, *direction, handshake.freeze().into()));
                            continue 'outer;
                        }
                        Poll::Ready(Some(Err(_))) | Poll::Ready(None) => {
                            return Poll::Ready(Some((
                                *peer,
                                HandshakeEvent::NegotiationError {
                                    peer: *peer,
                                    direction: *direction,
                                },
                            )));
                        }
                        Poll::Pending => continue 'outer,
                    },
                }
            }
        }

        if let Some((peer, direction, handshake)) = inner.ready.pop_front() {
            match direction {
                Direction::Outbound => {
                    let (substream, _, _) =
                        inner.substreams.remove(&(peer, direction)).expect("peer to exist");
                    return Poll::Ready(Some((
                        peer,
                        HandshakeEvent::OutboundNegotiated {
                            peer,
                            handshake,
                            substream,
                        },
                    )));
                }
                Direction::Inbound => {
                    let (substream, _, _) =
                        inner.substreams.remove(&(peer, direction)).expect("peer to exist");
                    return Poll::Ready(Some((
                        peer,
                        HandshakeEvent::InboundNegotiated {
                            peer,
                            handshake,
                            substream,
                        },
                    )));
                }
            }
        }

        Poll::Pending
    }
}
