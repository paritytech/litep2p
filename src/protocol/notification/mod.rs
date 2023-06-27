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
    error::{Error, SubstreamError},
    peer_id::PeerId,
    protocol::{
        notification::{
            negotiation::{HandshakeEvent, HandshakeService},
            types::{
                InnerNotificationEvent, NotificationCommand, NotificationError, NotificationSink,
                ValidationResult,
            },
        },
        ConnectionEvent, ConnectionService, Direction,
    },
    substream::{Substream, SubstreamSet},
    types::{protocol::ProtocolName, SubstreamId},
    TransportService,
};

use bytes::{Bytes, BytesMut};
use futures::{
    future::BoxFuture,
    stream::{select, FuturesUnordered, Select},
    SinkExt, StreamExt,
};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::{wrappers::ReceiverStream, StreamMap};

use std::{
    collections::{hash_map::Entry, HashMap},
    future::Future,
    pin::Pin,
};

mod negotiation;
#[cfg(test)]
mod tests;
pub mod types;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::protocol";

/// Default channel size for synchronous notifications.
const SYNC_CHANNEL_SIZE: usize = 2048;

/// Default channel size for asynchronous notifications.
const ASYNC_CHANNEL_SIZE: usize = 8;

#[derive(Debug)]
enum InnerNotificationError {
    /// Timeout for the operation was reached.
    Timeout,

    /// Failed to negotiate the protocol.
    NegotiationError {
        /// Peer ID.
        peer: PeerId,

        /// Substream ID.
        substream: SubstreamId,

        /// Error.
        error: Error,
    },
}

/// Type representing a pending outbound substream.
type PendingOutboundSubstream = Result<(PeerId, SubstreamId, Vec<u8>), InnerNotificationError>;

/// Inbound substream state.
#[derive(Debug)]
pub enum InboundState {
    /// Substream is closed.
    Closed,

    /// Handshake is being read from the remote peer.
    ReadingHandshake,

    /// Substream and its handshake are being validated by the user protocol.
    Validating(Box<dyn Substream>),

    /// Handshake is being sent to the remote peer.
    SendingHandshake,

    /// Substream is being accepted (handshake received and sent in one go)
    /// as the substream was already accepted.
    Accepting,

    /// Substream is open.
    Open(Box<dyn Substream>),
}

/// Outbound substream state.
#[derive(Debug)]
pub enum OutboundState {
    /// Substream is closed.
    Closed,

    /// Substream is in the state of being negotiated.
    ///
    /// This process entails sending local node's handshake and reading back the remote node's
    /// handshake if they've accepted the substream or detecting that the substream was closed
    /// in case the substream was rejected.
    Negotiating,

    /// Substream is open.
    Open {
        /// Received handshake.
        handshake: Vec<u8>,

        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },
}

// TODO: rename
#[derive(Debug)]
pub enum NewPeerState {
    /// Peer state is poisoned due to invalid state transition.
    Poisoned,

    /// Connection to peer is closed.
    Closed,

    /// Outbound substream initiated.
    OutboundInitiated {
        /// Substream ID.
        substream: SubstreamId,
    },

    /// Substream is being validated.
    Validating {
        /// Protocol.
        protocol: ProtocolName,

        /// Outbound protocol state.
        outbound: OutboundState,

        /// Inbound protocol state.
        inbound: InboundState,
    },

    /// Notification stream has been opened.
    Open {
        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },
}

// TODO: remove
/// Peer state
#[derive(Debug)]
enum PeerState {
    /// Peer state is poisoned.
    Poisoned,

    /// Notification stream is closed.
    Closed {
        /// Substream ID for a pending outbound substream.
        // TODO: documentation
        pending_open: Option<SubstreamId>,
    },

    /// Outbound substream initiated.
    OutboundInitiated {
        /// Substream ID.
        substream_id: SubstreamId,
    },

    /// Outbound substream is open, waiting for inbound substream to open.
    OutboundOpen {
        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },

    /// Inbound substream is open, waiting for outbound substream to be accepted.
    InboundOpen {
        /// Inbound substream.
        inbound: Box<dyn Substream>,
    },

    /// Inbound substream has been accepted, outbound substream has been initiated.
    InboundOpenOutboundInitiated {
        /// ID for the outbound substream.
        substream_id: SubstreamId,

        /// Inbound substream.
        inbound: Box<dyn Substream>,
    },

    /// Notification stream is open.
    Open {
        /// Outbound substream.
        outbound: Box<dyn Substream>,
    },
}

/// Peer context.
#[derive(Debug)]
struct PeerContext {
    /// Connection service.
    service: ConnectionService,

    /// Peer state.
    state: NewPeerState,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new(service: ConnectionService) -> Self {
        Self {
            service,
            state: NewPeerState::Closed,
        }
    }
}

#[derive(Debug)]
pub struct NotificationProtocol {
    /// Transport service.
    service: TransportService,

    /// Handshake bytes.
    handshake: Vec<u8>,

    /// TX channel passed to the protocol used for sending events.
    event_tx: Sender<InnerNotificationEvent>,

    /// RX channel passed to the protocol used for receiving commands.
    command_rx: Receiver<NotificationCommand>,

    /// Connected peers.
    peers: HashMap<PeerId, PeerContext>,

    /// Open substreams.
    substreams: SubstreamSet<PeerId>,

    /// Receivers.
    receivers: StreamMap<PeerId, Select<ReceiverStream<Vec<u8>>, ReceiverStream<Vec<u8>>>>,

    /// Pending outbound substreams.
    pending_outbound: FuturesUnordered<BoxFuture<'static, PendingOutboundSubstream>>,

    /// zzz
    negotiation: HandshakeService,
}

impl NotificationProtocol {
    pub fn new(service: TransportService, config: types::Config) -> Self {
        Self {
            service,
            peers: HashMap::new(),
            handshake: config.handshake.clone(),
            event_tx: config.event_tx,
            command_rx: config.command_rx,
            substreams: SubstreamSet::new(),
            receivers: StreamMap::new(),
            pending_outbound: FuturesUnordered::new(),
            negotiation: HandshakeService::new(config.handshake),
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(
        &mut self,
        peer: PeerId,
        service: ConnectionService,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                entry.insert(PeerContext::new(service));
                Ok(())
            }
            Entry::Occupied(_) => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?peer,
                    "state mismatch: peer already exists"
                );
                debug_assert!(false);
                Err(Error::PeerAlreadyExists(peer))
            }
        }
    }

    /// Connection closed to remote peer.
    async fn on_connection_closed(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        match self.peers.remove(&peer) {
            Some(_) => {
                self.substreams.remove(&peer);
                self.receivers.remove(&peer);

                self.event_tx
                    .send(InnerNotificationEvent::NotificationStreamClosed { peer })
                    .await
                    .map_err(From::from)
            }
            None => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?peer,
                    "state mismatch: peer doesn't exist"
                );
                debug_assert!(false);
                Err(Error::PeerDoesntExist(peer))
            }
        }
    }

    // /// Create a future which sends the local handshake to remote and reads the remote handshake back.
    // ///
    // /// If the remote handshake is read successfully, the substream is considered accepted.
    // fn negotiate_outbound(
    //     peer: PeerId,
    //     substream: SubstreamId,
    //     mut outbound: Box<dyn Substream>,
    //     handshake: Bytes,
    // ) -> Pin<Box<dyn Future<Output = PendingOutboundSubstream> + Send>> {
    //     // TODO: timeout
    //     Box::pin(async move {
    //         // send handshake to remote
    //         outbound.send(handshake).await.map_err(|error| {
    //             InnerNotificationError::NegotiationError {
    //                 peer,
    //                 substream,
    //                 error,
    //             }
    //         })?;

    //         // read remote's handshake
    //         match outbound.next().await {
    //             Some(Ok(handshake)) => Ok((peer, substream, handshake.freeze().into())),
    //             Some(Err(error)) => Err(InnerNotificationError::NegotiationError {
    //                 peer,
    //                 substream,
    //                 error,
    //             }),
    //             None => Err(InnerNotificationError::NegotiationError {
    //                 peer,
    //                 substream,
    //                 error: Error::SubstreamError(SubstreamError::ConnectionClosed),
    //             }),
    //         }
    //     })
    // }

    /// Local node opened a substream to remote node.
    async fn on_outbound_substream(
        &mut self,
        protocol: ProtocolName,
        peer: PeerId,
        substream_id: SubstreamId,
        outbound: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?protocol,
            ?peer,
            ?substream_id,
            "handle outbound substream",
        );

        // peer must exist since an outbound substream was received from them
        let context = self.peers.get_mut(&peer).expect("peer to exist");

        // the peer can be in two different states when an outbound substream has opened:
        //  - `NewPeerState::OutboundInitiated` - local node opened an outbound substream
        //  - `NewPeerState::Negotiating` - TODO
        match std::mem::replace(&mut context.state, NewPeerState::Poisoned) {
            NewPeerState::OutboundInitiated { substream } => {
                debug_assert!(substream == substream_id);

                // self.pending_outbound.push(Self::negotiate_outbound(
                //     peer,
                //     substream,
                //     outbound,
                //     self.handshake.clone().into(),
                // ));
                tracing::error!(target: LOG_TARGET, "outbound initiated, open substream");
                self.negotiation.negotiate_outbound(peer, outbound);
                context.state = NewPeerState::Validating {
                    protocol,
                    inbound: InboundState::Closed,
                    outbound: OutboundState::Negotiating,
                };
            }
            NewPeerState::Validating {
                protocol,
                inbound,
                outbound: _,
            } => match inbound {
                InboundState::SendingHandshake | InboundState::Open(_) => {
                    tracing::error!(
                        target: LOG_TARGET,
                        "outbound initiated, inbound validatinzzz"
                    );

                    context.state = NewPeerState::Validating {
                        protocol,
                        inbound,
                        outbound: OutboundState::Negotiating,
                    };
                    self.negotiation.negotiate_outbound(peer, outbound);
                }
                state => {
                    tracing::error!(target: LOG_TARGET, ?state, "inavlid state for Validating");
                    debug_assert!(false);
                }
            },
            state => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?state,
                    "invalid state for outbound substream"
                );
                debug_assert!(false);
            }
        }

        Ok(())
        // // TODO: check if entry exists in `pending_outbound` for `substream_id`
        // let _ = substream.send(self.handshake.clone().into()).await?;

        // // TODO: don't block here
        // match substream.next().await {
        //     Some(Ok(handshake)) => match std::mem::replace(&mut context.state, PeerState::Poisoned)
        //     {
        //         PeerState::OutboundInitiated {
        //             substream_id: stored_substream_id,
        //         } => {
        //             if substream_id != stored_substream_id {
        //                 tracing::warn!(
        //                     target: LOG_TARGET,
        //                     ?substream_id,
        //                     ?stored_substream_id,
        //                     "substream ID mismatch"
        //                 );
        //                 debug_assert!(false);
        //             }

        //             context.state = PeerState::OutboundOpen {
        //                 outbound: substream,
        //             };

        //             Ok(())
        //         }
        //         PeerState::InboundOpenOutboundInitiated {
        //             substream_id: stored_substream_id,
        //             inbound,
        //         } => {
        //             if substream_id != stored_substream_id {
        //                 tracing::warn!(
        //                     target: LOG_TARGET,
        //                     ?substream_id,
        //                     ?stored_substream_id,
        //                     "substream ID mismatch"
        //                 );
        //                 debug_assert!(false);
        //             }

        //             // set state to open and inform the user protocol about the stream
        //             context.state = PeerState::Open {
        //                 outbound: substream,
        //             };

        //             self.on_notification_stream_opened(peer, protocol, handshake, inbound)
        //                 .await
        //         }
        //         PeerState::Closed { pending_open } if pending_open == Some(substream_id) => {
        //             tracing::trace!(
        //                 target: LOG_TARGET,
        //                 ?peer,
        //                 ?substream_id,
        //                 "received stale outbound substream for closed connection"
        //             );
        //             context.state = PeerState::Closed { pending_open: None };
        //             Ok(())
        //         }
        //         state => {
        //             tracing::error!(
        //                 target: LOG_TARGET,
        //                 ?peer,
        //                 ?state,
        //                 "invalid state for {peer:?}: {state:?}"
        //             );
        //             Ok(())
        //         }
        //     },
        //     Some(Err(error)) => {
        //         tracing::debug!(
        //             target: LOG_TARGET,
        //             ?protocol,
        //             ?peer,
        //             ?error,
        //             "failed to read handshake from inbound substream"
        //         );
        //         Err(Error::SubstreamError(SubstreamError::ConnectionClosed))
        //     }
        //     None => {
        //         let _ = self
        //             .event_tx
        //             .send(InnerNotificationEvent::NotificationStreamOpenFailure {
        //                 peer,
        //                 error: NotificationError::Rejected,
        //             })
        //             .await;
        //         Err(Error::SubstreamError(SubstreamError::ConnectionClosed))
        //     }
        // }
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        protocol: ProtocolName,
        peer: PeerId,
        inbound: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?protocol,
            "handle inbound substream"
        );

        // peer must exist since an inbound substream was received from them
        let context = self.peers.get_mut(&peer).expect("peer to exist");

        // the peer can be in two different states when an outbound substream has opened:
        //  - `NewPeerState::Closed` - remote node opened a substream to local node and it needs to be validated
        //  - `NewPeerState::` - TODO
        match std::mem::replace(&mut context.state, NewPeerState::Poisoned) {
            NewPeerState::Closed => {
                tracing::warn!(
                    target: LOG_TARGET,
                    "state is closed, read handshake and return it to user for validation"
                );
                self.negotiation.read_handshake(peer, inbound);
                context.state = NewPeerState::Validating {
                    protocol,
                    inbound: InboundState::ReadingHandshake,
                    outbound: OutboundState::Closed,
                };
            }
            NewPeerState::Validating {
                protocol,
                outbound,
                inbound: InboundState::Closed,
            } => {
                tracing::info!(target: LOG_TARGET, "handle state for `Validating`");

                self.negotiation.accept_inbound(peer, inbound);
                context.state = NewPeerState::Validating {
                    protocol,
                    outbound,
                    inbound: InboundState::Accepting,
                };
            }
            state => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?state,
                    "invalid state for inbound state"
                );
                debug_assert!(false);
            }
        }

        Ok(())

        // // TODO: don't block here
        // match substream.next().await {
        //     Some(Ok(handshake)) => match std::mem::replace(&mut context.state, PeerState::Poisoned)
        //     {
        //         PeerState::Closed { .. } => {
        //             context.state = PeerState::InboundOpen { inbound: substream };
        //             self.event_tx
        //                 .send(InnerNotificationEvent::ValidateSubstream {
        //                     protocol,
        //                     peer,
        //                     handshake: handshake.into(),
        //                 })
        //                 .await
        //                 .map_err(From::from)
        //         }
        //         PeerState::OutboundOpen { outbound } => {
        //             // acknowledge substream by sending our handshake, set the state to open
        //             // and inform the user protocol about the stream
        //             substream.send(self.handshake.clone().into()).await.unwrap();
        //             context.state = PeerState::Open { outbound };

        //             self.on_notification_stream_opened(peer, protocol, handshake, substream)
        //                 .await
        //         }
        //         state => {
        //             tracing::error!(target: LOG_TARGET, ?peer, ?state, "invalid state");
        //             Ok(())
        //         }
        //     },
        //     Some(Err(error)) => {
        //         tracing::debug!(
        //             target: LOG_TARGET,
        //             ?protocol,
        //             ?peer,
        //             ?error,
        //             "failed to read handshake from inbound substream"
        //         );
        //         Err(Error::SubstreamError(SubstreamError::ConnectionClosed))
        //     }
        //     None => {
        //         tracing::debug!(
        //             target: LOG_TARGET,
        //             ?protocol,
        //             ?peer,
        //             "read `None` from inbound substream"
        //         );
        //         Err(Error::SubstreamError(SubstreamError::ConnectionClosed))
        //     }
        // }
    }

    /// Failed to open substream to remote peer.
    fn on_substream_open_failure(&mut self, substream: SubstreamId, error: Error) {
        tracing::debug!(
            target: LOG_TARGET,
            ?substream,
            ?error,
            "failed to open substream"
        );
    }

    /// Notification stream has been opened, initialize context for the stream and inform user.
    ///
    /// The peer must be in in state `PeerState::Open` as it indicates that both substreams
    /// (inbound and oubound) are open. Other states are considered invalid state machine transitions.
    async fn on_notification_stream_opened(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        handshake: BytesMut,
        inbound: Box<dyn Substream>,
    ) -> crate::Result<()> {
        Ok(())
        // let Some(context) = self.peers.get_mut(&peer) else {
        //     tracing::error!(target: LOG_TARGET, "invalid state: notification stream opened but peer doesn't exist");
        //     debug_assert!(false);
        //     return Err(Error::PeerDoesntExist(peer));
        // };

        // match &mut context.state {
        //     PeerState::Open { .. } => {
        //         let (async_tx, async_rx) = channel(ASYNC_CHANNEL_SIZE);
        //         let (sync_tx, sync_rx) = channel(SYNC_CHANNEL_SIZE);
        //         let notif_stream =
        //             select(ReceiverStream::new(async_rx), ReceiverStream::new(sync_rx));
        //         let sink = NotificationSink::new(sync_tx, async_tx);

        //         self.substreams.insert(peer, inbound);
        //         self.receivers.insert(peer, notif_stream);

        //         // ignore error as the user protocol may have exited.
        //         self.event_tx
        //             .send(InnerNotificationEvent::NotificationStreamOpened {
        //                 protocol,
        //                 peer,
        //                 sink,
        //                 handshake: handshake.into(),
        //             })
        //             .await
        //             .map_err(From::from)
        //     }
        //     _ => {
        //         tracing::error!(
        //             target: LOG_TARGET,
        //             "invalid state: notification stream opened but peer doesn't exist"
        //         );
        //         debug_assert!(false);
        //         // TODO: introduce new error
        //         Ok(())
        //     }
        // }
    }

    /// Open substream to remote `peer`.
    async fn on_open_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::debug!(target: LOG_TARGET, ?peer, "no open connection to peer");

            // TODO: convert this to a function call
            let _ = self
                .event_tx
                .send(InnerNotificationEvent::NotificationStreamOpenFailure {
                    peer,
                    error: NotificationError::NoConnection,
                })
                .await;
            return Err(Error::PeerDoesntExist(peer))
        };

        // protocol can only request a new outbound substream to be opened if the state is `Closed`
        if let NewPeerState::Closed = std::mem::replace(&mut context.state, NewPeerState::Poisoned)
        {
            let substream = context.service.open_substream().await?;
            context.state = NewPeerState::OutboundInitiated { substream };
        }

        Ok(())

        // match self.peers.get_mut(&peer) {
        //     None => {
        //         tracing::debug!(target: LOG_TARGET, ?peer, "no open connection to peer");

        //         let _ = self
        //             .event_tx
        //             .send(InnerNotificationEvent::NotificationStreamOpenFailure {
        //                 peer,
        //                 error: NotificationError::NoConnection,
        //             })
        //             .await;
        //         Err(Error::PeerDoesntExist(peer))
        //     }
        //     Some(context) => {
        //         let substream_id = context.service.open_substream().await?;
        //         context.state = PeerState::OutboundInitiated { substream_id };
        //         Ok(())
        //     }
        // }
    }

    /// Close substream to remote `peer`.
    // TODO: add documentation
    async fn on_close_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        Ok(())
        // tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        // match self.peers.get_mut(&peer) {
        //     None => Err(Error::PeerDoesntExist(peer)),
        //     Some(context) => {
        //         self.receivers.remove(&peer);
        //         self.substreams.remove(&peer);

        //         match std::mem::replace(&mut context.state, PeerState::Poisoned) {
        //             PeerState::OutboundOpen {
        //                 outbound: mut substream,
        //             }
        //             | PeerState::InboundOpen {
        //                 inbound: mut substream,
        //             }
        //             | PeerState::Open {
        //                 outbound: mut substream,
        //             } => substream.close().await.unwrap(),
        //             PeerState::InboundOpenOutboundInitiated {
        //                 inbound: mut substream,
        //                 substream_id,
        //             } => {
        //                 substream.close().await.unwrap();
        //                 context.state = PeerState::Closed {
        //                     pending_open: Some(substream_id),
        //                 };
        //             }
        //             PeerState::OutboundInitiated { substream_id } => {
        //                 context.state = PeerState::Closed {
        //                     pending_open: Some(substream_id),
        //                 };
        //             }
        //             state => {
        //                 tracing::error!(target: LOG_TARGET, ?state, "invalid state for peer");
        //                 debug_assert!(false);
        //             }
        //         }

        //         Ok(())
        //     }
        // }
    }

    /// Handle validation result.
    // TODO: add documentation
    async fn on_validation_result(
        &mut self,
        peer: PeerId,
        result: ValidationResult,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?result,
            "handle validation result"
        );

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::debug!(target: LOG_TARGET, ?peer, "peer doesn't exist");
            return  Err(Error::PeerDoesntExist(peer));
        };

        match std::mem::replace(&mut context.state, NewPeerState::Poisoned) {
            NewPeerState::Validating {
                protocol,
                outbound,
                inbound: InboundState::Validating(mut inbound),
            } => match result {
                ValidationResult::Reject => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "substream rejected and closed");

                    let _ = inbound.close().await;
                    context.state = NewPeerState::Closed;

                    Ok(())
                }
                ValidationResult::Accept => async {
                    self.negotiation.send_handshake(peer, inbound);
                    context.service.open_substream().await.map_err(From::from)
                }
                .await
                .map(|substream_id| {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        ?substream_id,
                        "substream accepted and outbound substream opened"
                    );

                    context.state = NewPeerState::Validating {
                        protocol,
                        inbound: InboundState::SendingHandshake,
                        outbound,
                    };
                })
                .map_err(|error| {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        "failed to accept substream, now closed"
                    );

                    context.state = NewPeerState::Closed;
                    error
                }),
            },
            state => {
                tracing::trace!(target: LOG_TARGET, ?peer, ?state, "??????????????????????");

                debug_assert!(false);
                Ok(())
            }
        }
        // match std::mem::replace(&mut context.state, PeerState::Poisoned) {
        //     PeerState::InboundOpen { mut inbound } => match result {
        //     ValidationResult::Reject => {
        //         tracing::trace!(target: LOG_TARGET, ?peer, "substream rejected and closed");

        //         let _ = inbound.close().await;
        //         context.state = PeerState::Closed { pending_open: None };

        //         Ok(())
        //     }
        //     ValidationResult::Accept => async {
        //         inbound.send(self.handshake.clone().into()).await?;
        //         context.service.open_substream().await.map_err(From::from)
        //     }
        //     .await
        //     .map(|substream_id| {
        //         tracing::trace!(
        //             target: LOG_TARGET,
        //             ?peer,
        //             "substream accepted and outbound substream opened"
        //         );

        //         context.state = PeerState::InboundOpenOutboundInitiated {
        //             substream_id,
        //             inbound,
        //         };
        //     })
        //     .map_err(|error| {
        //         tracing::trace!(
        //             target: LOG_TARGET,
        //             ?peer,
        //             "failed to accept substream, now closed"
        //         );

        //         context.state = PeerState::Closed { pending_open: None };
        //         error
        //     }),
        // },
        //     state => {
        //         tracing::error!(
        //             target: LOG_TARGET,
        //             ?state,
        //             "invalid state for peer, ignoring validation result"
        //         );
        //         context.state = state;
        //         debug_assert!(false);
        //         // TODO: introduce new error for invalid state transition
        //         Ok(())
        //     }
        // }
    }

    /// Handle substream event.
    async fn on_substream_event(
        &mut self,
        peer: PeerId,
        message: crate::Result<BytesMut>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, is_ok = ?message.is_ok(), "handle substream event");

        // // TODO: check peer state here at some point to validate handshake

        let event = match message {
            Ok(message) => InnerNotificationEvent::NotificationReceived {
                peer,
                notification: message.freeze().into(),
            },
            Err(_) => {
                self.substreams.remove(&peer);
                self.receivers.remove(&peer);
                self.peers
                    .get_mut(&peer)
                    .expect("peer to exist since an event was received")
                    .state = NewPeerState::Closed;

                InnerNotificationEvent::NotificationStreamClosed { peer }
            }
        };

        self.event_tx.send(event).await.map_err(From::from)
    }

    /// Handle negotiation event.
    async fn on_negotiation_event(
        &mut self,
        peer: PeerId,
        event: HandshakeEvent,
    ) -> crate::Result<()> {
        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::error!(target: LOG_TARGET, "invalid state: notification stream opened but peer doesn't exist");
            debug_assert!(false);
            return Err(Error::PeerDoesntExist(peer));
        };

        tracing::info!(target: LOG_TARGET, ?event, state = ?context.state, "handle negotiation event");

        match event {
            HandshakeEvent::OutboundNegotiated {
                peer,
                handshake,
                substream,
            } => {
                tracing::error!(target: LOG_TARGET, "outbound negotiated");
                self.negotiation.remove_outbound(&peer);

                match std::mem::replace(&mut context.state, NewPeerState::Poisoned) {
                    NewPeerState::Validating {
                        protocol,
                        outbound: OutboundState::Negotiating,
                        inbound,
                    } => {
                        context.state = NewPeerState::Validating {
                            protocol,
                            outbound: OutboundState::Open {
                                handshake,
                                outbound: substream,
                            },
                            inbound,
                        };
                    }
                    event => tracing::error!(target: LOG_TARGET, ?event, "ehrezzz"),
                }
            }
            HandshakeEvent::OutboundNegotiationError { peer } => {
                tracing::error!(target: LOG_TARGET, "error");
                self.negotiation.remove_outbound(&peer);
            }
            HandshakeEvent::InboundNegotiated {
                peer,
                handshake,
                substream,
            } => {
                tracing::error!(target: LOG_TARGET, "inbound negotiated {handshake:?}");
                self.negotiation.remove_inbound(&peer);

                match std::mem::replace(&mut context.state, NewPeerState::Poisoned) {
                    NewPeerState::Validating {
                        protocol,
                        outbound,
                        inbound: InboundState::ReadingHandshake,
                    } => {
                        context.state = NewPeerState::Validating {
                            protocol: protocol.clone(),
                            inbound: InboundState::Validating(substream),
                            outbound,
                        };

                        tracing::error!(target: LOG_TARGET, "send for validation");

                        // TODO: function
                        let _: crate::Result<_> = self
                            .event_tx
                            .send(InnerNotificationEvent::ValidateSubstream {
                                protocol,
                                peer,
                                handshake: handshake.into(),
                            })
                            .await
                            .map_err(From::from);
                    }
                    NewPeerState::Validating {
                        protocol,
                        inbound: InboundState::SendingHandshake,
                        outbound,
                    } => {
                        tracing::error!(target: LOG_TARGET, "sending handshake");

                        context.state = NewPeerState::Validating {
                            protocol: protocol.clone(),
                            inbound: InboundState::Open(substream),
                            outbound,
                        };
                    }
                    state => {
                        tracing::error!(target: LOG_TARGET, ?state, "inavlid state, again");
                        debug_assert!(false);
                    }
                }
            }
            HandshakeEvent::InboundAccepted { peer, substream } => {
                match std::mem::replace(&mut context.state, NewPeerState::Poisoned) {
                    NewPeerState::Validating {
                        protocol,
                        outbound,
                        inbound: InboundState::Accepting,
                    } => {
                        tracing::error!(target: LOG_TARGET, "INBOUND SUBSTRAM ACCEPTED");
                        context.state = NewPeerState::Validating {
                            protocol,
                            outbound,
                            inbound: InboundState::Open(substream),
                        };
                    }
                    state => {
                        tracing::error!(target: LOG_TARGET, ?state, "inavlid state, again 2222");
                        debug_assert!(false);
                    }
                }
            }
            HandshakeEvent::InboundNegotiationError { peer } => {
                tracing::error!(target: LOG_TARGET, "inbound negotiation error");
                self.negotiation.remove_inbound(&peer);
            }
        }

        match std::mem::replace(&mut context.state, NewPeerState::Poisoned) {
            NewPeerState::Validating {
                protocol,
                outbound:
                    OutboundState::Open {
                        handshake,
                        outbound,
                    },
                inbound: InboundState::Open(inbound),
            } => {
                let (async_tx, async_rx) = channel(ASYNC_CHANNEL_SIZE);
                let (sync_tx, sync_rx) = channel(SYNC_CHANNEL_SIZE);
                let notif_stream =
                    select(ReceiverStream::new(async_rx), ReceiverStream::new(sync_rx));
                let sink = NotificationSink::new(sync_tx, async_tx);

                self.substreams.insert(peer, inbound);
                self.receivers.insert(peer, notif_stream);

                context.state = NewPeerState::Open { outbound };

                // ignore error as the user protocol may have exited.
                let _: crate::Result<_> = self
                    .event_tx
                    .send(InnerNotificationEvent::NotificationStreamOpened {
                        protocol,
                        peer,
                        sink,
                        handshake: handshake.into(),
                    })
                    .await
                    .map_err(From::from);
            }
            state => context.state = state,
        }

        Ok(())
    }

    /// Start [`NotificationProtocol`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting request-response event loop");

        loop {
            tokio::select! {
                event = self.service.next_event() => match event {
                    Some(ConnectionEvent::ConnectionEstablished { peer, service }) => {
                        if let Err(error) = self.on_connection_established(peer, service).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to register peer",
                            );
                        }
                    }
                    Some(ConnectionEvent::ConnectionClosed { peer }) => {
                        if let Err(error) = self.on_connection_closed(peer).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to disconnect peer",
                            );
                        }
                    }
                    Some(ConnectionEvent::SubstreamOpened {
                        peer,
                        substream,
                        direction,
                        protocol,
                    }) => match direction {
                        Direction::Inbound => {
                            if let Err(error) = self.on_inbound_substream(protocol, peer, substream).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to handle inbound substream",
                                );
                            }
                        }
                        Direction::Outbound(substream_id) => {
                            if let Err(error) = self
                                .on_outbound_substream(protocol, peer, substream_id, substream)
                                .await
                            {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to handle outbound substream",
                                );
                            }
                        }
                    },
                    Some(ConnectionEvent::SubstreamOpenFailure { substream, error }) => {
                        self.on_substream_open_failure(substream, error);
                    }
                    None => return,
                },
                command = self.command_rx.recv() => match command {
                    None => {
                        tracing::debug!(target: LOG_TARGET, "user protocol has exited, exiting");
                        return
                    }
                    Some(command) => match command {
                        NotificationCommand::OpenSubstream { peer } => {
                            if let Err(error) = self.on_open_substream(peer).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to open substream"
                                );
                            }
                        }
                        NotificationCommand::CloseSubstream { peer } => {
                            if let Err(error) = self.on_close_substream(peer).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to open substream"
                                );
                            }
                        }
                        NotificationCommand::SubstreamValidated { peer, result } => {
                            if let Err(error) = self.on_validation_result(peer, result).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to open substream"
                                );
                            }
                        }
                        NotificationCommand::SetHandshake { handshake } => {
                            self.handshake = handshake;
                        }
                    }
                },
                event = self.substreams.next(), if !self.substreams.is_empty() => {
                    let (peer, event) = event.expect("`SubstreamSet` to return `Some(..)`");

                    if let Err(error) = self.on_substream_event(peer, event).await {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to handle event from substream",
                        );
                    }
                }
                event = self.pending_outbound.select_next_some(), if !self.pending_outbound.is_empty() => {
                    match event {
                        event => {
                            tracing::error!(target: LOG_TARGET, ?event, "handle outbound substream event");
                        }
                    }
                },
                event = self.negotiation.next(), if !self.negotiation.is_empty() => {
                    let (peer, event) = event.expect("`HandshakeService` to return `Some(..)`");

                    if let Err(error) = self.on_negotiation_event(peer, event).await {
                        tracing::error!(target: LOG_TARGET, ?error, "failed to handle negotiation event");
                    }
                }
                event = self.receivers.next(), if !self.receivers.is_empty() => match event {
                    Some((peer, notification)) => {
                        tracing::info!(target: LOG_TARGET, ?peer, "send notification to peer");

                        match self.peers.get_mut(&peer) {
                            Some(context) => match &mut context.state {
                                NewPeerState::Open { outbound } => {
                                    // TODO: handle error
                                    let _result = outbound.send(notification.into()).await;
                                }
                                state => tracing::error!(target: LOG_TARGET, ?state, "invalid state for peer"),
                            }
                            None => {} // TODO: handle error
                        }
                    }
                    None => {
                        tracing::info!(target: LOG_TARGET, "here");
                    }
                }
            }
        }
    }
}
