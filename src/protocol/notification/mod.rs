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

//! Notification protocol implementation.

use crate::{
    error::Error,
    protocol::{
        notification::{
            connection::Connection,
            handle::{NotificationEventHandle, NotificationSink},
            negotiation::{HandshakeEvent, HandshakeService},
            types::{NotificationCommand, ASYNC_CHANNEL_SIZE, SYNC_CHANNEL_SIZE},
        },
        Direction, Transport, TransportEvent, TransportService,
    },
    substream::Substream,
    types::{protocol::ProtocolName, SubstreamId},
    PeerId, DEFAULT_CHANNEL_SIZE,
};

use futures::{SinkExt, StreamExt};
use tokio::sync::{
    mpsc::{channel, Receiver, Sender},
    oneshot,
};

use std::collections::{hash_map::Entry, HashMap};

pub use config::{Config, ConfigBuilder};
pub use handle::NotificationHandle;
pub use types::{NotificationError, NotificationEvent, ValidationResult};

mod config;
mod connection;
mod handle;
mod negotiation;
mod types;

#[cfg(test)]
mod tests;

/// Logging target for the file.
const LOG_TARGET: &str = "notification::protocol";

/// Inbound substream state.
#[derive(Debug)]
enum InboundState {
    /// Substream is closed.
    Closed,

    /// Handshake is being read from the remote node.
    ReadingHandshake,

    /// Substream and its handshake are being validated by the user protocol.
    Validating {
        /// Inbound substream.
        inbound: Substream,
    },

    /// Handshake is being sent to the remote node.
    SendingHandshake,

    /// Substream is open.
    Open {
        /// Inbound substream.
        inbound: Substream,
    },
}

/// Outbound substream state.
#[derive(Debug)]
enum OutboundState {
    /// Substream is closed.
    Closed,

    /// Outbound substream initiated.
    OutboundInitiated {
        /// Substream ID.
        substream: SubstreamId,
    },

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
        outbound: Substream,
    },
}

impl OutboundState {
    /// Get pending outboud substream ID, if it exists.
    fn pending_open(&self) -> Option<SubstreamId> {
        match &self {
            OutboundState::OutboundInitiated { substream } => Some(*substream),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum PeerState {
    /// Peer state is poisoned due to invalid state transition.
    Poisoned,

    /// Connection to peer is closed.
    Closed {
        /// Connection might have been closed while there was an outbound substream still pending.
        ///
        /// To handle this state transition correctly in case the substream opens after the
        /// connection is considered closed, store the `SubstreamId` to that it can be verified in
        /// case the substream ever opens.
        pending_open: Option<SubstreamId>,
    },

    /// Outbound substream initiated.
    OutboundInitiated {
        /// Substream ID.
        substream: SubstreamId,
    },

    /// Substream is being validated.
    Validating {
        /// Protocol.
        protocol: ProtocolName,

        /// Fallback protocol, if the substream was negotiated using a fallback name.
        fallback: Option<ProtocolName>,

        /// Outbound protocol state.
        outbound: OutboundState,

        /// Inbound protocol state.
        inbound: InboundState,
    },

    /// Notification stream has been opened.
    Open {
        /// `Oneshot::Sender` for shutting down the connection.
        shutdown: oneshot::Sender<()>,
    },
}

/// Peer context.
#[derive(Debug)]
struct PeerContext {
    /// Peer state.
    state: PeerState,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new() -> Self {
        Self {
            state: PeerState::Closed { pending_open: None },
        }
    }
}

#[derive(Debug)]
pub(crate) struct NotificationProtocol {
    /// Transport service.
    service: TransportService,

    /// Handshake bytes.
    handshake: Vec<u8>,

    /// Auto accept inbound substream if the outbound substream was initiated by the local node.
    auto_accept: bool,

    /// TX channel passed to the protocol used for sending events.
    event_handle: NotificationEventHandle,

    /// TX channel given to substream handlers for sending notifications to user.
    notif_tx: Sender<(PeerId, Vec<u8>)>,

    /// TX channel for sending shut down notifications from connection handlers to
    /// [`NotificationProtocol`].
    shutdown_tx: Sender<PeerId>,

    /// RX channel for receiving shutdown notifications from the connection handlers.
    shutdown_rx: Receiver<PeerId>,

    /// RX channel passed to the protocol used for receiving commands.
    command_rx: Receiver<NotificationCommand>,

    /// Connected peers.
    peers: HashMap<PeerId, PeerContext>,

    /// Pending outboudn substreams.
    pending_outbound: HashMap<SubstreamId, PeerId>,

    /// Handshaking service which reads and writes the handshakes to inbound
    /// and outbound substreams asynchronously.
    negotiation: HandshakeService,
}

impl NotificationProtocol {
    pub(crate) fn new(service: TransportService, config: Config) -> Self {
        let (shutdown_tx, shutdown_rx) = channel(DEFAULT_CHANNEL_SIZE);

        Self {
            service,
            shutdown_tx,
            shutdown_rx,
            peers: HashMap::new(),
            handshake: config.handshake.clone(),
            auto_accept: config.auto_accept,
            event_handle: NotificationEventHandle::new(config.event_tx),
            notif_tx: config.notif_tx,
            command_rx: config.command_rx,
            pending_outbound: HashMap::new(),
            negotiation: HandshakeService::new(config.handshake),
        }
    }

    /// Connection established to remote node.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                entry.insert(PeerContext::new());
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

    /// Connection closed to remote node.
    ///
    /// If the connection was considered open (both substreams were open), user is notified that
    /// the notification stream was closed.
    ///
    /// If the connection was still in progress (either substream was not fully open), the user is
    /// reported about it only if they had opened an outbound substream (outbound is either fully
    /// open, it had been initiated or the substream was under negotiation).
    async fn on_connection_closed(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        let Some(context) = self.peers.remove(&peer) else {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                "state mismatch: peer doesn't exist"
            );
            debug_assert!(false);
            return Err(Error::PeerDoesntExist(peer));
        };

        // clean up all pending state for the peer
        self.negotiation.remove_outbound(&peer);
        self.negotiation.remove_inbound(&peer);

        match context.state {
            // outbound initiated, report open failure to peer
            PeerState::OutboundInitiated { .. } => {
                self.event_handle
                    .report_notification_stream_open_failure(peer, NotificationError::Rejected)
                    .await;
            }
            // substream fully open, report that the notification stream is closed
            PeerState::Open { shutdown } => {
                let _ = shutdown.send(());
                self.event_handle.report_notification_stream_closed(peer).await;
            }
            // if the substream was being validated, notify user only if the outbound state is not
            // `Closed` states other than `Closed` indicate that the user was interested
            // in opening the substream as well and so that user can have correct state
            // tracking, it must be notified of this
            PeerState::Validating { outbound, .. }
                if !std::matches!(outbound, OutboundState::Closed) =>
            {
                self.event_handle
                    .report_notification_stream_open_failure(peer, NotificationError::Rejected)
                    .await;
            }
            _ => {}
        }

        Ok(())
    }

    /// Local node opened a substream to remote node.
    ///
    /// The connection can be in three different states:
    ///   - this is the first substream that was opened and thus the connection was initiated by the
    ///     local node
    ///   - this is a response to a previously received inbound substream which the local node
    ///     accepted and as a result, opened its own substream
    ///   - local and remote nodes opened substreams at the same time
    ///
    /// In the first case, the local node's handshake is sent to remote node and the substream is
    /// polled in the background until they either send their handshake or close the substream.
    ///
    /// For the second case, the connection was initiated by the remote node and the substream was
    /// accepted by the local node which initiated an outbound substream to the remote node.
    /// The only valid states for this case are [`InboundState::Open`],
    /// and [`InboundState::SendingHandshake`] as they imply
    /// that the inbound substream have been accepted by the local node and this opened outbound
    /// substream is a result of a valid state transition.
    ///
    /// For the third case, if the nodes have opened substreams at the same time, the outbound state
    /// must be [`OutbounState::OutboundInitiated`] to ascertain that the an outbound substream was
    /// actually opened. Any other state would be a state mismatch and would mean that the
    /// connection is opening substreams without the permission of the protocol handler.
    async fn on_outbound_substream(
        &mut self,
        protocol: ProtocolName,
        fallback: Option<ProtocolName>,
        peer: PeerId,
        substream_id: SubstreamId,
        mut outbound: Substream,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?protocol,
            ?substream_id,
            "handle outbound substream",
        );

        // peer must exist since an outbound substream was received from them
        let context = self.peers.get_mut(&peer).expect("peer to exist");
        let pending_peer = self.pending_outbound.remove(&substream_id);

        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            // the connection was initiated by the local node, send handshake to remote and wait to
            // receive their handshake back
            PeerState::OutboundInitiated { substream } => {
                debug_assert!(substream == substream_id);
                debug_assert!(pending_peer == Some(peer));

                self.negotiation.negotiate_outbound(peer, outbound);
                context.state = PeerState::Validating {
                    protocol,
                    fallback,
                    inbound: InboundState::Closed,
                    outbound: OutboundState::Negotiating,
                };
            }
            PeerState::Validating {
                protocol,
                fallback,
                inbound,
                outbound: outbound_state,
            } => {
                // the inbound substream has been accepted by the local node since the handshake has
                // been read and the local handshake has either already been sent or
                // it's in the process of being sent.
                match inbound {
                    InboundState::SendingHandshake | InboundState::Open { .. } => {
                        context.state = PeerState::Validating {
                            protocol,
                            fallback,
                            inbound,
                            outbound: OutboundState::Negotiating,
                        };
                        self.negotiation.negotiate_outbound(peer, outbound);
                    }
                    // nodes have opened substreams at the same time
                    inbound_state => match outbound_state {
                        OutboundState::OutboundInitiated { substream } => {
                            debug_assert!(substream == substream_id);

                            context.state = PeerState::Validating {
                                protocol,
                                fallback,
                                inbound: inbound_state,
                                outbound: OutboundState::Negotiating,
                            };
                            self.negotiation.negotiate_outbound(peer, outbound);
                        }
                        // invalid state: more than one outbound substream has been opened
                        inner_state => {
                            tracing::error!(
                                target: LOG_TARGET,
                                ?inbound_state,
                                ?inner_state,
                                "invalid state, expected `OutboundInitiated`",
                            );

                            let _ = outbound.close().await;
                            debug_assert!(false);
                        }
                    },
                }
            }
            // the connection may have been closed while an outbound substream was pending
            // if the outbound substream was initiated successfully, close it and reset
            // `pending_open`
            PeerState::Closed { pending_open } if pending_open == Some(substream_id) => {
                let _ = outbound.close().await;

                context.state = PeerState::Closed { pending_open: None };
            }
            state => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?peer,
                    ?protocol,
                    ?substream_id,
                    ?state,
                    "invalid state: more than one outbound substream opened",
                );

                let _ = outbound.close().await;
                debug_assert!(false);
            }
        }

        Ok(())
    }

    /// Remote opened a substream to local node.
    ///
    /// The peer can be in three different states for the inbound substream to be considered valid:
    ///   - the connection is closed
    ///   - outbound substream has been opened but not yet acknowledged by the remote peer
    ///   - outbound substream has been opened and acknowledged by the remote peer and it's being
    ///     negotiated
    ///
    /// If remote opened more than one substream, the new substream is simply discarded.
    async fn on_inbound_substream(
        &mut self,
        protocol: ProtocolName,
        fallback: Option<ProtocolName>,
        peer: PeerId,
        mut substream: Substream,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?protocol,
            "handle inbound substream"
        );

        // peer must exist since an inbound substream was received from them
        let context = self.peers.get_mut(&peer).expect("peer to exist");

        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            // the peer state is closed so this is a fresh inbound substream.
            PeerState::Closed { .. } => {
                self.negotiation.read_handshake(peer, substream);

                context.state = PeerState::Validating {
                    protocol,
                    fallback,
                    inbound: InboundState::ReadingHandshake,
                    outbound: OutboundState::Closed,
                };
            }
            // if the connection is under validation (so an outbound substream has been opened and
            // it's still pending or under negotiation), the only valid state for the
            // inbound state is closed as it indicates that there isn't an inbound substream yet for
            // the remote node duplicate substreams are prohibited.
            PeerState::Validating {
                protocol,
                fallback,
                outbound,
                inbound: InboundState::Closed,
            } => {
                self.negotiation.read_handshake(peer, substream);

                context.state = PeerState::Validating {
                    protocol,
                    fallback,
                    outbound,
                    inbound: InboundState::ReadingHandshake,
                };
            }
            // outbound substream may have been initiated by the local node while a remote node also
            // opened a substream roughly at the same time
            PeerState::OutboundInitiated {
                substream: outbound,
            } => {
                self.negotiation.read_handshake(peer, substream);

                context.state = PeerState::Validating {
                    protocol,
                    fallback,
                    outbound: OutboundState::OutboundInitiated {
                        substream: outbound,
                    },
                    inbound: InboundState::ReadingHandshake,
                };
            }
            // remote opened another inbound substream, close it and otherwise ignore the event
            // as this is a non-serious protocol violation.
            state => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    ?protocol,
                    ?fallback,
                    ?state,
                    "remote opened more than one inbound substreams, discarding"
                );

                let _ = substream.close().await;
                context.state = state;
            }
        }

        Ok(())
    }

    /// Failed to open substream to remote node.
    ///
    /// If the substream was initiated by the local node, it must be reported that the substream
    /// failed to open. Otherwise the peer state can silently be converted to `Closed`.
    async fn on_substream_open_failure(&mut self, substream: SubstreamId, error: Error) {
        tracing::debug!(
            target: LOG_TARGET,
            ?substream,
            ?error,
            "failed to open substream"
        );

        let Some(peer) = self.pending_outbound.remove(&substream) else {
            tracing::warn!(target: LOG_TARGET, ?substream, "pending outbound substream doesn't exist");
            debug_assert!(false);
            return;
        };

        // peer must exist since an outbound substream failure was received from them
        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::warn!(target: LOG_TARGET, ?peer, "peer doesn't exist");
            debug_assert!(false);
            return;
        };

        match &mut context.state {
            PeerState::OutboundInitiated { .. } => {
                context.state = PeerState::Closed { pending_open: None };
                self.event_handle
                    .report_notification_stream_open_failure(peer, NotificationError::Rejected)
                    .await;
            }
            // if the substream was accepted by the local node and as a result, an outbound
            // substream was accepted as a result this should not be reported to local node
            PeerState::Validating { outbound, .. } => {
                self.negotiation.remove_inbound(&peer);
                self.negotiation.remove_outbound(&peer);

                if !std::matches!(outbound, OutboundState::Closed) {
                    self.event_handle
                        .report_notification_stream_open_failure(peer, NotificationError::Rejected)
                        .await;
                }
            }
            state => {
                tracing::warn!(target: LOG_TARGET, ?state, "invalid state for outbound substream open failure");
                context.state = PeerState::Closed { pending_open: None };
                debug_assert!(false);
            }
        }
    }

    /// Open substream to remote `peer`.
    ///
    /// Outbound substream can opened only if the `PeerState` is `Closed`.
    /// By forcing the substream to be opened only if the state is currently closed,
    /// `NotificationProtocol` can enfore more predictable state transitions.
    ///
    /// Other states either imply an invalid state transition ([`PeerState::Open`]) or that an
    /// inbound substream has already been received and its currently being validated by the user.
    async fn on_open_substream(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::warn!(target: LOG_TARGET, ?peer, "no open connection to peer");

            self.event_handle
                .report_notification_stream_open_failure(peer, NotificationError::NoConnection)
                .await;
            return Err(Error::PeerDoesntExist(peer));
        };

        // protocol can only request a new outbound substream to be opened if the state is `Closed`
        // other states imply that it's already open
        if let PeerState::Closed { .. } = std::mem::replace(&mut context.state, PeerState::Poisoned)
        {
            match self.service.open_substream(peer).await {
                Ok(substream) => {
                    self.pending_outbound.insert(substream, peer);
                    context.state = PeerState::OutboundInitiated { substream };
                }
                Err(error) => {
                    tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to open substream");

                    self.event_handle
                        .report_notification_stream_open_failure(
                            peer,
                            NotificationError::NoConnection,
                        )
                        .await;
                    context.state = PeerState::Closed { pending_open: None };
                }
            }
        }

        Ok(())
    }

    /// Close substream to remote `peer`.
    ///
    /// This function can only be called if the substream was actually open, any other state is
    /// unreachable as the user is unable to emit this command to [`NotificationProtocol`] unless
    /// the connection has been fully opened.
    async fn on_close_substream(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "close substream");

        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::debug!(target: LOG_TARGET, ?peer, "peer doesn't exist");
            return;
        };

        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::Open { shutdown } => {
                let _ = shutdown.send(());
                context.state = PeerState::Closed { pending_open: None };

                self.event_handle.report_notification_stream_closed(peer).await;
            }
            _ => unreachable!(),
        }
    }

    /// Handle validation result.
    ///
    /// The validation result binary (accept/reject). If the node is rejected, the substreams are
    /// discarded and state is set to `PeerState::Closed`. If there was an outbound substream in
    /// progress while the connection was rejected by the user, the oubound state is discarded,
    /// except for the substream ID of the substream which is kept for later use, in case the
    /// substream happens to open.
    ///
    /// If the node is accepted and there is no outbound substream to them open yet, a new substream
    /// is opened and once it opens, the local handshake will be sent to the remote peer and if
    /// they also accept the substream the connection is considered fully open.
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
            return Err(Error::PeerDoesntExist(peer));
        };

        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::Validating {
                protocol,
                fallback,
                outbound,
                inbound: InboundState::Validating { mut inbound },
            } => match result {
                // substream was rejected by the local node, if an outbound substream was under
                // negotation, discard that data and if an outbound substream was
                // initiated, save the `SubstreamId` of that substream and later if the substream
                // is opened, the state can be corrected to `pending_open: None`.
                ValidationResult::Reject => {
                    let _ = inbound.close().await;
                    self.negotiation.remove_outbound(&peer);
                    self.negotiation.remove_inbound(&peer);
                    context.state = PeerState::Closed {
                        pending_open: outbound.pending_open(),
                    };

                    Ok(())
                }
                ValidationResult::Accept => match outbound {
                    // no outbound substream exists so initiate a new substream open and send the
                    // local handshake to remote node, indicating that the
                    // connection was accepted by the local node
                    OutboundState::Closed => match self.service.open_substream(peer).await {
                        Ok(substream) => {
                            self.negotiation.send_handshake(peer, inbound);

                            context.state = PeerState::Validating {
                                protocol,
                                fallback,
                                inbound: InboundState::SendingHandshake,
                                outbound: OutboundState::OutboundInitiated { substream },
                            };
                            Ok(())
                        }
                        // since the `OutboundState` was `Closed`, it means that the substream was
                        // initiated by the remote node this means that a failure to open a
                        // substream should not be reported to the local node as it's not the one
                        // that initiated the substream, even if it accepted the connection.
                        Err(error) => {
                            let _ = inbound.close().await;
                            context.state = PeerState::Closed { pending_open: None };

                            Err(error)
                        }
                    },
                    // here the state is one of `OutboundState::{OutboundInitiated, Negotiating,
                    // Open}` so that state can be safely ignored and all that
                    // has to be done is to send the local handshake to remote
                    // node to indicate that the connection was accepted.
                    _ => {
                        self.negotiation.send_handshake(peer, inbound);

                        context.state = PeerState::Validating {
                            protocol,
                            fallback,
                            inbound: InboundState::SendingHandshake,
                            outbound,
                        };
                        Ok(())
                    }
                },
            },
            // if the user incorrectly send a validation result for a peer that doesn't require
            // validation, set state back to what it was and add a `debug_assert!(false)` so it can
            // be catched in tests and user can fix the logic in their code
            state => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?state,
                    "validation result received for peer that doesn't require validation");

                context.state = state;
                debug_assert!(false);
                Ok(())
            }
        }
    }

    /// Handle handshake event.
    ///
    /// There are three different handshake event types:
    ///   - outbound substream negotiated
    ///   - inbound substream negotiated
    ///   - substream negotiation error
    ///
    /// Neither outbound nor inbound substream negotiated automatically means that the connection is
    /// considered open as both substreams must be fully negotiated for that to be the case. That is
    /// why the peer state for inbound and outbound are set separately and at the end of the
    /// function is the collective state of the substreams checked and if both substreams are
    /// negotiated, the user informed that the connection is open.
    ///
    /// If the negotiation fails, the user may have to be informed of that. Outbound substream
    /// failure always results in user getting notified since the existence of an outbound substream
    /// means that the user has either initiated an outbound substreams or has accepted an inbound
    /// substreams, resulting in an outbound substreams.
    ///
    /// Negotiation failure for inbound substreams which are in the state
    /// [`InboundState::ReadingHandshake`] don't result in any notification because while the
    /// handshake is being read from the substream, the user is oblivious to the fact that an
    /// inbound substream has even been received.
    async fn on_handshake_event(&mut self, peer: PeerId, event: HandshakeEvent) {
        let Some(context) = self.peers.get_mut(&peer) else {
            tracing::error!(target: LOG_TARGET, "invalid state: negotiation event received but peer doesn't exist");
            debug_assert!(false);
            return;
        };

        // TODO: try to get inbound and outbound state here if possible?

        match event {
            // outbound substream was negotiated, the only valid state for peer is `Validating`
            // and only valid state for `OutboundState` is `Negotiating`
            HandshakeEvent::OutboundNegotiated {
                peer,
                handshake,
                substream,
            } => {
                self.negotiation.remove_outbound(&peer);

                match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                    PeerState::Validating {
                        protocol,
                        fallback,
                        outbound: OutboundState::Negotiating,
                        inbound,
                    } => {
                        context.state = PeerState::Validating {
                            protocol,
                            fallback,
                            outbound: OutboundState::Open {
                                handshake,
                                outbound: substream,
                            },
                            inbound,
                        };
                    }
                    state => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?peer,
                            ?state,
                            "outbound substream negotiated but peer has invalid state"
                        );
                        debug_assert!(false);
                    }
                }
            }
            // inbound negotiation event completed
            //
            // the negotiation event can be on of two different types:
            //   - remote handshake was read from the substream
            //   - local handshake has been sent to remote node
            //
            // For the first case, the substream has to be validated by the local node.
            // This means reporting the protocol name, potential negotiated fallback and the
            // handshake. Local node will then either accept or reject the substream which is
            // handled by [`NotificationProtocol::on_validation_result()`]. Compared to
            // Substrate, litep2p requires both peers to validate the inbound handshake to allow
            // more complex connection validation. If this is not necessary and the protocol wishes
            // to auto-accept the inbound substreams that are a result of an outbound substream
            // already accepted by the remote node, the substrem validation is skipped and the local
            // handshake is sent right away.
            //
            // For the second case, the local handshake was sent to remote node successfully and the
            // inbound substream is considered open and if the outbound substream is open as well,
            // the connection is fully open.
            //
            // Only valid states for [`InboundState`] are [`InboundState::ReadingHandshake`] and
            // [`InboundState::SendingHandshake`] because otherwise the inbound
            // substream cannot be in [`HandshakeService`](super::negotiation::HandshakeService)
            // unless there is a logic bug in the state machine.
            HandshakeEvent::InboundNegotiated {
                peer,
                handshake,
                substream,
            } => {
                self.negotiation.remove_inbound(&peer);

                match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                    PeerState::Validating {
                        protocol,
                        fallback,
                        outbound,
                        inbound: InboundState::ReadingHandshake,
                    } => {
                        if !std::matches!(outbound, OutboundState::Closed) && self.auto_accept {
                            self.negotiation.send_handshake(peer, substream);
                            context.state = PeerState::Validating {
                                protocol,
                                fallback,
                                inbound: InboundState::SendingHandshake,
                                outbound,
                            };

                            return;
                        }

                        context.state = PeerState::Validating {
                            protocol: protocol.clone(),
                            fallback: fallback.clone(),
                            inbound: InboundState::Validating { inbound: substream },
                            outbound,
                        };

                        self.event_handle
                            .report_inbound_substream(protocol, fallback, peer, handshake.into())
                            .await;
                    }
                    PeerState::Validating {
                        protocol,
                        fallback,
                        inbound: InboundState::SendingHandshake,
                        outbound,
                    } => {
                        context.state = PeerState::Validating {
                            protocol: protocol.clone(),
                            fallback: fallback.clone(),
                            inbound: InboundState::Open { inbound: substream },
                            outbound,
                        };
                    }
                    _state => debug_assert!(false),
                }
            }
            // error occurred during negotiation, eitehr for inbound or outbound substream
            // user is notified of the error only if they've either initiated an outbound substream
            // or if they accepted an inbound substream and as a result initiated an outbound
            // substream.
            HandshakeEvent::NegotiationError { peer, direction } => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?direction,
                    state = ?context.state,
                    "failed to negotiate outbound substream"
                );
                let _ = self.negotiation.remove_outbound(&peer);
                let _ = self.negotiation.remove_inbound(&peer);

                // if an outbound substream had been initiated (whatever its state is), it means
                // that the user knows about the connection and must be notified that it failed to
                // negotiate.
                match std::mem::replace(&mut context.state, PeerState::Poisoned) {
                    PeerState::Validating { outbound, .. } => {
                        // notify user if the outbound substream is not considered closed
                        if !std::matches!(outbound, OutboundState::Closed) {
                            return self
                                .event_handle
                                .report_notification_stream_open_failure(
                                    peer,
                                    NotificationError::Rejected,
                                )
                                .await;
                        }

                        context.state = PeerState::Closed {
                            pending_open: outbound.pending_open(),
                        };
                    }
                    _state => debug_assert!(false),
                }
            }
        }

        // if both inbound and outbound substreams are considered open, notify the user that
        // a notification stream has been opened and set up for sending and receiving
        // notifications to and from remote node
        match std::mem::replace(&mut context.state, PeerState::Poisoned) {
            PeerState::Validating {
                protocol,
                fallback,
                outbound:
                    OutboundState::Open {
                        handshake,
                        outbound,
                    },
                inbound: InboundState::Open { inbound },
            } => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    ?protocol,
                    ?fallback,
                    "notification stream opened"
                );

                let (async_tx, async_rx) = channel(ASYNC_CHANNEL_SIZE);
                let (sync_tx, sync_rx) = channel(SYNC_CHANNEL_SIZE);
                let sink = NotificationSink::new(peer, sync_tx, async_tx);

                // start connection handler for the peer which only deals with sending/receiving
                // notifications
                let shutdown_tx = self.shutdown_tx.clone();
                let notif_tx = self.notif_tx.clone();
                let (connection, shutdown) = Connection::new(
                    peer,
                    inbound,
                    outbound,
                    notif_tx.clone(),
                    shutdown_tx.clone(),
                    async_rx,
                    sync_rx,
                );
                tokio::spawn(connection.start());

                context.state = PeerState::Open { shutdown };
                self.event_handle
                    .report_notification_stream_opened(
                        protocol,
                        fallback,
                        peer,
                        handshake.into(),
                        sink,
                    )
                    .await;
            }
            state => context.state = state,
        }
    }

    /// Handle next notification event.
    async fn next_event(&mut self) {
        // biased select is used because the substream events must be prioritized above other events
        // that is becaused a closed substream is detected by either `substreams` or `negotiation`
        // and if that event is not handled with priority but, e.g., inbound substream is
        // handled before, it can create a situation where the state machine gets confused
        // about the peer's state.
        tokio::select! {
            biased;

            event = self.negotiation.next(), if !self.negotiation.is_empty() => {
                let (peer, event) = event.expect("`HandshakeService` to return `Some(..)`");
                self.on_handshake_event(peer, event).await;
            }
            event = self.shutdown_rx.recv() => match event {
                None => return,
                Some(peer) => {
                    self.peers
                        .get_mut(&peer)
                        .expect("peer to exist since an event was received")
                        .state = PeerState::Closed { pending_open: None };
                    self.event_handle.report_notification_stream_closed(peer).await;
                }
            },
            event = self.service.next_event() => match event {
                Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                    if let Err(error) = self.on_connection_established(peer).await {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to register peer",
                        );
                    }
                }
                Some(TransportEvent::ConnectionClosed { peer }) => {
                    if let Err(error) = self.on_connection_closed(peer).await {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to disconnect peer",
                        );
                    }
                }
                Some(TransportEvent::SubstreamOpened {
                    peer,
                    substream,
                    direction,
                    protocol,
                    fallback,
                }) => match direction {
                    Direction::Inbound => {
                        if let Err(error) = self.on_inbound_substream(protocol, fallback, peer, substream).await {
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
                            .on_outbound_substream(protocol, fallback, peer, substream_id, substream)
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
                Some(TransportEvent::SubstreamOpenFailure { substream, error }) => {
                    self.on_substream_open_failure(substream, error).await;
                }
                Some(TransportEvent::DialFailure { .. }) => {},
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
                        self.on_close_substream(peer).await;
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
                        self.negotiation.set_handshake(handshake.clone());
                        self.handshake = handshake;
                    }
                }
            },
        }
    }

    /// Start [`NotificationProtocol`] event loop.
    pub(crate) async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting notification event loop");

        loop {
            self.next_event().await;
        }
    }
}
