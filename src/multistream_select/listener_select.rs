// Copyright 2017 Parity Technologies (UK) Ltd.
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

//! Protocol negotiation strategies for the peer acting as the listener
//! in a multistream-select protocol negotiation.

use crate::{
    codec::unsigned_varint::UnsignedVarint,
    error::{self, Error},
    multistream_select::{
        protocol::{
            webrtc_encode_multistream_message, HeaderLine, Message, MessageIO, Protocol,
            ProtocolError,
        },
        Negotiated, NegotiationError,
    },
    types::protocol::ProtocolName,
};

use bytes::{Bytes, BytesMut};
use futures::prelude::*;
use smallvec::SmallVec;
use std::{
    convert::TryFrom as _,
    iter::FromIterator,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

const LOG_TARGET: &str = "litep2p::multistream-select";

/// Returns a `Future` that negotiates a protocol on the given I/O stream
/// for a peer acting as the _listener_ (or _responder_).
///
/// This function is given an I/O stream and a list of protocols and returns a
/// computation that performs the protocol negotiation with the remote. The
/// returned `Future` resolves with the name of the negotiated protocol and
/// a [`Negotiated`] I/O stream.
pub fn listener_select_proto<R, I>(inner: R, protocols: I) -> ListenerSelectFuture<R, I::Item>
where
    R: AsyncRead + AsyncWrite,
    I: IntoIterator,
    I::Item: AsRef<[u8]>,
{
    let protocols = protocols.into_iter().filter_map(|n| match Protocol::try_from(n.as_ref()) {
        Ok(p) => Some((n, p)),
        Err(e) => {
            tracing::warn!(
                target: LOG_TARGET,
                "Listener: Ignoring invalid protocol: {} due to {}",
                String::from_utf8_lossy(n.as_ref()),
                e
            );
            None
        }
    });
    ListenerSelectFuture {
        protocols: SmallVec::from_iter(protocols),
        state: State::RecvHeader {
            io: MessageIO::new(inner),
        },
        last_sent_na: false,
    }
}

/// The `Future` returned by [`listener_select_proto`] that performs a
/// multistream-select protocol negotiation on an underlying I/O stream.
#[pin_project::pin_project]
pub struct ListenerSelectFuture<R, N> {
    protocols: SmallVec<[(N, Protocol); 8]>,
    state: State<R, N>,
    /// Whether the last message sent was a protocol rejection (i.e. `na\n`).
    ///
    /// If the listener reads garbage or EOF after such a rejection,
    /// the dialer is likely using `V1Lazy` and negotiation must be
    /// considered failed, but not with a protocol violation or I/O
    /// error.
    last_sent_na: bool,
}

enum State<R, N> {
    RecvHeader {
        io: MessageIO<R>,
    },
    SendHeader {
        io: MessageIO<R>,
    },
    RecvMessage {
        io: MessageIO<R>,
    },
    SendMessage {
        io: MessageIO<R>,
        message: Message,
        protocol: Option<N>,
    },
    Flush {
        io: MessageIO<R>,
        protocol: Option<N>,
    },
    Done,
}

impl<R, N> Future for ListenerSelectFuture<R, N>
where
    // The Unpin bound here is required because we
    // produce a `Negotiated<R>` as the output.
    // It also makes the implementation considerably
    // easier to write.
    R: AsyncRead + AsyncWrite + Unpin,
    N: AsRef<[u8]> + Clone,
{
    type Output = Result<(N, Negotiated<R>), NegotiationError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        loop {
            match mem::replace(this.state, State::Done) {
                State::RecvHeader { mut io } => {
                    match io.poll_next_unpin(cx) {
                        Poll::Ready(Some(Ok(Message::Header(h)))) => match h {
                            HeaderLine::V1 => *this.state = State::SendHeader { io },
                        },
                        Poll::Ready(Some(Ok(_))) =>
                            return Poll::Ready(Err(ProtocolError::InvalidMessage.into())),
                        Poll::Ready(Some(Err(err))) => return Poll::Ready(Err(From::from(err))),
                        // Treat EOF error as [`NegotiationError::Failed`], not as
                        // [`NegotiationError::ProtocolError`], allowing dropping or closing an I/O
                        // stream as a permissible way to "gracefully" fail a negotiation.
                        Poll::Ready(None) => return Poll::Ready(Err(NegotiationError::Failed)),
                        Poll::Pending => {
                            *this.state = State::RecvHeader { io };
                            return Poll::Pending;
                        }
                    }
                }

                State::SendHeader { mut io } => {
                    match Pin::new(&mut io).poll_ready(cx) {
                        Poll::Pending => {
                            *this.state = State::SendHeader { io };
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(err)) => return Poll::Ready(Err(From::from(err))),
                    }

                    let msg = Message::Header(HeaderLine::V1);
                    if let Err(err) = Pin::new(&mut io).start_send(msg) {
                        return Poll::Ready(Err(From::from(err)));
                    }

                    *this.state = State::Flush { io, protocol: None };
                }

                State::RecvMessage { mut io } => {
                    let msg = match Pin::new(&mut io).poll_next(cx) {
                        Poll::Ready(Some(Ok(msg))) => msg,
                        // Treat EOF error as [`NegotiationError::Failed`], not as
                        // [`NegotiationError::ProtocolError`], allowing dropping or closing an I/O
                        // stream as a permissible way to "gracefully" fail a negotiation.
                        //
                        // This is e.g. important when a listener rejects a protocol with
                        // [`Message::NotAvailable`] and the dialer does not have alternative
                        // protocols to propose. Then the dialer will stop the negotiation and drop
                        // the corresponding stream. As a listener this EOF should be interpreted as
                        // a failed negotiation.
                        Poll::Ready(None) => return Poll::Ready(Err(NegotiationError::Failed)),
                        Poll::Pending => {
                            *this.state = State::RecvMessage { io };
                            return Poll::Pending;
                        }
                        Poll::Ready(Some(Err(err))) => {
                            if *this.last_sent_na {
                                // When we read garbage or EOF after having already rejected a
                                // protocol, the dialer is most likely using `V1Lazy` and has
                                // optimistically settled on this protocol, so this is really a
                                // failed negotiation, not a protocol violation. In this case
                                // the dialer also raises `NegotiationError::Failed` when finally
                                // reading the `N/A` response.
                                if let ProtocolError::InvalidMessage = &err {
                                    tracing::trace!(
                                        target: LOG_TARGET,
                                        "Listener: Negotiation failed with invalid \
                                        message after protocol rejection."
                                    );
                                    return Poll::Ready(Err(NegotiationError::Failed));
                                }
                                if let ProtocolError::IoError(e) = &err {
                                    if e.kind() == std::io::ErrorKind::UnexpectedEof {
                                        tracing::trace!(
                                            target: LOG_TARGET,
                                            "Listener: Negotiation failed with EOF \
                                            after protocol rejection."
                                        );
                                        return Poll::Ready(Err(NegotiationError::Failed));
                                    }
                                }
                            }

                            return Poll::Ready(Err(From::from(err)));
                        }
                    };

                    match msg {
                        Message::ListProtocols => {
                            let supported =
                                this.protocols.iter().map(|(_, p)| p).cloned().collect();
                            let message = Message::Protocols(supported);
                            *this.state = State::SendMessage {
                                io,
                                message,
                                protocol: None,
                            }
                        }
                        Message::Protocol(p) => {
                            let protocol = this.protocols.iter().find_map(|(name, proto)| {
                                if &p == proto {
                                    Some(name.clone())
                                } else {
                                    None
                                }
                            });

                            let message = if protocol.is_some() {
                                tracing::debug!("Listener: confirming protocol: {}", p);
                                Message::Protocol(p.clone())
                            } else {
                                tracing::debug!(
                                    "Listener: rejecting protocol: {}",
                                    String::from_utf8_lossy(p.as_ref())
                                );
                                Message::NotAvailable
                            };

                            *this.state = State::SendMessage {
                                io,
                                message,
                                protocol,
                            };
                        }
                        _ => return Poll::Ready(Err(ProtocolError::InvalidMessage.into())),
                    }
                }

                State::SendMessage {
                    mut io,
                    message,
                    protocol,
                } => {
                    match Pin::new(&mut io).poll_ready(cx) {
                        Poll::Pending => {
                            *this.state = State::SendMessage {
                                io,
                                message,
                                protocol,
                            };
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(())) => {}
                        Poll::Ready(Err(err)) => return Poll::Ready(Err(From::from(err))),
                    }

                    if let Message::NotAvailable = &message {
                        *this.last_sent_na = true;
                    } else {
                        *this.last_sent_na = false;
                    }

                    if let Err(err) = Pin::new(&mut io).start_send(message) {
                        return Poll::Ready(Err(From::from(err)));
                    }

                    *this.state = State::Flush { io, protocol };
                }

                State::Flush { mut io, protocol } => {
                    match Pin::new(&mut io).poll_flush(cx) {
                        Poll::Pending => {
                            *this.state = State::Flush { io, protocol };
                            return Poll::Pending;
                        }
                        Poll::Ready(Ok(())) => {
                            // If a protocol has been selected, finish negotiation.
                            // Otherwise expect to receive another message.
                            match protocol {
                                Some(protocol) => {
                                    tracing::debug!(
                                        "Listener: sent confirmed protocol: {}",
                                        String::from_utf8_lossy(protocol.as_ref())
                                    );
                                    let io = Negotiated::completed(io.into_inner());
                                    return Poll::Ready(Ok((protocol, io)));
                                }
                                None => *this.state = State::RecvMessage { io },
                            }
                        }
                        Poll::Ready(Err(err)) => return Poll::Ready(Err(From::from(err))),
                    }
                }

                State::Done => panic!("State::poll called after completion"),
            }
        }
    }
}

/// Result of [`webrtc_listener_negotiate()`].
#[derive(Debug)]
pub enum ListenerSelectResult {
    /// Requested protocol is available and substream can be accepted.
    Accepted {
        /// Protocol that is confirmed.
        protocol: ProtocolName,

        /// `multistream-select` message.
        message: Bytes,
    },

    /// Requested protocol is not available.
    Rejected {
        /// `multistream-select` message.
        message: Bytes,
    },

    /// The multistream-select header was received but no protocol was proposed yet.
    /// The caller should send the `message` (header echo) and wait for the next payload.
    PendingProtocol {
        /// `multistream-select` message (header echo).
        message: Bytes,
    },
}

/// Decode a single varint-length-prefixed multistream-select message from `data`,
/// advancing past the consumed bytes.
fn decode_multistream_message(data: &mut Bytes) -> Result<Message, error::NegotiationError> {
    let (len, tail) = unsigned_varint::decode::usize(data).map_err(|error| {
        tracing::debug!(
            target: LOG_TARGET,
            ?error,
            message = ?data,
            "Failed to decode length-prefix in multistream message",
        );
        error::NegotiationError::ParseError(error::ParseError::InvalidData)
    })?;

    if len > tail.len() {
        tracing::debug!(
            target: LOG_TARGET,
            length_prefix = len,
            actual_length = tail.len(),
            "Truncated multistream message",
        );
        return Err(error::NegotiationError::ParseError(
            error::ParseError::InvalidData,
        ));
    }

    let len_size = data.len() - tail.len();
    let payload = data.slice(len_size..len_size + len);
    *data = data.slice(len_size + len..);

    Message::decode(payload).map_err(|error| {
        tracing::debug!(target: LOG_TARGET, ?error, "Failed to decode multistream message");
        error::NegotiationError::ParseError(error::ParseError::InvalidData)
    })
}

/// Negotiate protocols for listener.
///
/// Parse the protocol offered by the remote peer and check if it matches any locally available
/// protocol. The `header_received` parameter indicates whether the multistream-select header
/// has already been exchanged in a previous round.
pub fn webrtc_listener_negotiate(
    supported_protocols: Vec<ProtocolName>,
    mut payload: Bytes,
    header_received: bool,
) -> crate::Result<ListenerSelectResult> {
    // Save for zero-copy header echo (Bytes::clone is O(1)).
    let raw_payload = payload.clone();

    let first_msg = decode_multistream_message(&mut payload)?;

    let (protocol, header_in_this_payload) = match first_msg {
        Message::Header(HeaderLine::V1) => {
            if payload.is_empty() {
                // Header only — echo the exact received bytes back (zero alloc).
                return Ok(ListenerSelectResult::PendingProtocol {
                    message: raw_payload,
                });
            }
            // Header + protocol in same payload.
            match decode_multistream_message(&mut payload)? {
                Message::Protocol(protocol) => (protocol, true),
                _ =>
                    return Err(Error::NegotiationError(
                        error::NegotiationError::ParseError(error::ParseError::InvalidData),
                    )),
            }
        }
        // Protocol without header is only valid if the header was already exchanged.
        Message::Protocol(protocol) if header_received => (protocol, false),
        _ =>
            return Err(Error::NegotiationError(
                error::NegotiationError::MultistreamSelectError(NegotiationError::Failed),
            )),
    };

    // Reject messages with unexpected trailing data.
    if !payload.is_empty() {
        return Err(Error::NegotiationError(
            error::NegotiationError::ParseError(error::ParseError::InvalidData),
        ));
    }

    tracing::trace!(
        target: LOG_TARGET,
        protocol = ?std::str::from_utf8(protocol.as_ref()),
        "listener: checking protocol",
    );

    for supported in supported_protocols.iter() {
        if protocol.as_ref() == supported.as_bytes() {
            return Ok(ListenerSelectResult::Accepted {
                protocol: supported.clone(),
                message: webrtc_encode_multistream_message(
                    Message::Protocol(protocol),
                    header_in_this_payload,
                )?
                .freeze(),
            });
        }
    }

    tracing::trace!(
        target: LOG_TARGET,
        "listener: handshake rejected, no supported protocol found",
    );

    Ok(ListenerSelectResult::Rejected {
        message: webrtc_encode_multistream_message(Message::NotAvailable, header_in_this_payload)?
            .freeze(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error;
    use bytes::BufMut;

    #[test]
    fn webrtc_listener_negotiate_works() {
        let local_protocols = vec![
            ProtocolName::from("/13371338/proto/1"),
            ProtocolName::from("/sup/proto/1"),
            ProtocolName::from("/13371338/proto/2"),
            ProtocolName::from("/13371338/proto/3"),
            ProtocolName::from("/13371338/proto/4"),
        ];
        let message = webrtc_encode_multistream_message(
            Message::Protocol(Protocol::try_from(&b"/13371338/proto/1"[..]).unwrap()),
            true,
        )
        .unwrap()
        .freeze();

        match webrtc_listener_negotiate(local_protocols, message, false) {
            Err(error) => panic!("error received: {error:?}"),
            Ok(ListenerSelectResult::Rejected { .. }) => panic!("message rejected"),
            Ok(ListenerSelectResult::PendingProtocol { .. }) => panic!("unexpected pending"),
            Ok(ListenerSelectResult::Accepted { protocol, .. }) => {
                assert_eq!(protocol, ProtocolName::from("/13371338/proto/1"));
            }
        }
    }

    #[test]
    fn invalid_message() {
        let local_protocols = vec![
            ProtocolName::from("/13371338/proto/1"),
            ProtocolName::from("/sup/proto/1"),
            ProtocolName::from("/13371338/proto/2"),
            ProtocolName::from("/13371338/proto/3"),
            ProtocolName::from("/13371338/proto/4"),
        ];
        let message = webrtc_encode_multistream_message(
            Message::Protocols(vec![
                Protocol::try_from(&b"/13371338/proto/1"[..]).unwrap(),
                Protocol::try_from(&b"/sup/proto/1"[..]).unwrap(),
            ]),
            true,
        )
        .unwrap()
        .freeze();

        match webrtc_listener_negotiate(local_protocols, message, false) {
            Err(error) => assert!(std::matches!(
                error,
                Error::NegotiationError(error::NegotiationError::ParseError(
                    error::ParseError::InvalidData
                )),
            )),
            _ => panic!("invalid event"),
        }
    }

    #[test]
    fn only_header_line_received() {
        let local_protocols = vec![
            ProtocolName::from("/13371338/proto/1"),
            ProtocolName::from("/sup/proto/1"),
            ProtocolName::from("/13371338/proto/2"),
            ProtocolName::from("/13371338/proto/3"),
            ProtocolName::from("/13371338/proto/4"),
        ];

        // Send only header line with varint length prefix.
        let mut bytes = BytesMut::with_capacity(32);
        Message::Header(HeaderLine::V1).encode(&mut bytes).unwrap();
        let payload = Bytes::from(UnsignedVarint::encode(bytes).unwrap());

        match webrtc_listener_negotiate(local_protocols, payload.clone(), false) {
            Ok(ListenerSelectResult::PendingProtocol { message }) => {
                assert_eq!(message, payload);
            }
            event => panic!("invalid event: {event:?}"),
        }
    }

    #[test]
    fn header_line_missing() {
        let local_protocols = vec![
            ProtocolName::from("/13371338/proto/1"),
            ProtocolName::from("/sup/proto/1"),
            ProtocolName::from("/13371338/proto/2"),
            ProtocolName::from("/13371338/proto/3"),
            ProtocolName::from("/13371338/proto/4"),
        ];

        // Single protocol, no header.
        let mut bytes = BytesMut::with_capacity(64);
        Message::Protocol(Protocol::try_from(&b"/13371338/proto/1"[..]).unwrap())
            .encode(&mut bytes)
            .unwrap();
        let payload = Bytes::from(UnsignedVarint::encode(bytes).unwrap());

        match webrtc_listener_negotiate(local_protocols, payload, false) {
            Err(error) => assert!(std::matches!(
                error,
                Error::NegotiationError(error::NegotiationError::MultistreamSelectError(
                    NegotiationError::Failed
                ))
            )),
            event => panic!("invalid event: {event:?}"),
        }
    }

    #[test]
    fn protocol_not_supported() {
        let local_protocols = vec![
            ProtocolName::from("/13371338/proto/1"),
            ProtocolName::from("/sup/proto/1"),
            ProtocolName::from("/13371338/proto/2"),
            ProtocolName::from("/13371338/proto/3"),
            ProtocolName::from("/13371338/proto/4"),
        ];
        let message = webrtc_encode_multistream_message(
            Message::Protocol(Protocol::try_from(&b"/13371339/proto/1"[..]).unwrap()),
            true,
        )
        .unwrap()
        .freeze();

        match webrtc_listener_negotiate(local_protocols, message, false) {
            Err(error) => panic!("error received: {error:?}"),
            Ok(ListenerSelectResult::Rejected { message }) => {
                assert_eq!(
                    message,
                    webrtc_encode_multistream_message(Message::NotAvailable, true)
                        .unwrap()
                        .freeze()
                );
            }
            Ok(ListenerSelectResult::Accepted { .. }) => panic!("message accepted"),
            Ok(ListenerSelectResult::PendingProtocol { .. }) => panic!("unexpected pending"),
        }
    }

    #[test]
    fn protocols_not_supported() {
        let local_protocols = vec![ProtocolName::from("/13371338/proto/1")];

        // Round 1: send header only → PendingProtocol (header echo).
        let mut bytes = BytesMut::with_capacity(32);
        Message::Header(HeaderLine::V1).encode(&mut bytes).unwrap();
        let header_payload = Bytes::from(UnsignedVarint::encode(bytes).unwrap());

        match webrtc_listener_negotiate(local_protocols.clone(), header_payload.clone(), false) {
            Ok(ListenerSelectResult::PendingProtocol { message }) => {
                assert_eq!(message, header_payload);
            }
            event => panic!("expected PendingProtocol, got {event:?}"),
        }

        // Round 2: send first protocol (not supported) → Rejected (na, no header).
        let mut bytes = BytesMut::with_capacity(64);
        Message::Protocol(Protocol::try_from(&b"/unsupported/proto/1"[..]).unwrap())
            .encode(&mut bytes)
            .unwrap();
        let proto1_payload = Bytes::from(UnsignedVarint::encode(bytes).unwrap());

        match webrtc_listener_negotiate(local_protocols.clone(), proto1_payload, true) {
            Ok(ListenerSelectResult::Rejected { message }) => {
                assert_eq!(
                    message,
                    webrtc_encode_multistream_message(Message::NotAvailable, false)
                        .unwrap()
                        .freeze()
                );
            }
            event => panic!("expected Rejected, got {event:?}"),
        }

        // Round 3: send second protocol (also not supported) → Rejected (na, no header).
        let mut bytes = BytesMut::with_capacity(64);
        Message::Protocol(Protocol::try_from(&b"/unsupported/proto/2"[..]).unwrap())
            .encode(&mut bytes)
            .unwrap();
        let proto2_payload = Bytes::from(UnsignedVarint::encode(bytes).unwrap());

        match webrtc_listener_negotiate(local_protocols, proto2_payload, true) {
            Ok(ListenerSelectResult::Rejected { message }) => {
                assert_eq!(
                    message,
                    webrtc_encode_multistream_message(Message::NotAvailable, false)
                        .unwrap()
                        .freeze()
                );
            }
            event => panic!("expected Rejected, got {event:?}"),
        }
    }

    #[test]
    fn header_only_then_protocol() {
        let local_protocols = vec![ProtocolName::from("/13371338/proto/1")];

        // Call 1: header only → PendingProtocol.
        let mut bytes = BytesMut::with_capacity(32);
        Message::Header(HeaderLine::V1).encode(&mut bytes).unwrap();
        let header_payload = Bytes::from(UnsignedVarint::encode(bytes).unwrap());

        match webrtc_listener_negotiate(local_protocols.clone(), header_payload.clone(), false) {
            Ok(ListenerSelectResult::PendingProtocol { message }) => {
                assert_eq!(message, header_payload);
            }
            event => panic!("expected PendingProtocol, got {event:?}"),
        }

        // Call 2: protocol only (header_received=true) → Accepted.
        let mut bytes = BytesMut::with_capacity(64);
        Message::Protocol(Protocol::try_from(&b"/13371338/proto/1"[..]).unwrap())
            .encode(&mut bytes)
            .unwrap();
        let proto_payload = Bytes::from(UnsignedVarint::encode(bytes).unwrap());

        match webrtc_listener_negotiate(local_protocols, proto_payload, true) {
            Ok(ListenerSelectResult::Accepted { protocol, .. }) => {
                assert_eq!(protocol, ProtocolName::from("/13371338/proto/1"));
            }
            event => panic!("expected Accepted, got {event:?}"),
        }
    }
}
