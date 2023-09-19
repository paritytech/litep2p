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

//! Request-response protocol implementation.

use crate::{
    error::{Error, SubstreamError},
    protocol::{
        request_response::handle::RequestResponseCommand, Direction, Transport, TransportEvent,
        TransportService,
    },
    substream::Substream,
    types::{protocol::ProtocolName, RequestId, SubstreamId},
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, SinkExt, StreamExt};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::timeout,
};

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    io::ErrorKind,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

pub use config::{Config, ConfigBuilder};
pub use handle::{DialOptions, RequestResponseError, RequestResponseEvent, RequestResponseHandle};

mod config;
mod handle;

/// Logging target for the file.
const LOG_TARGET: &str = "request-response::protocol";

/// Default request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Pending request.
type PendingRequest = (PeerId, RequestId, Result<Vec<u8>, RequestResponseError>);

/// Request context.
struct RequestContext {
    /// Peer ID.
    peer: PeerId,

    /// Request ID.
    request_id: RequestId,

    /// Request.
    request: Vec<u8>,
}

impl RequestContext {
    /// Create new [`RequestContext`].
    fn new(peer: PeerId, request_id: RequestId, request: Vec<u8>) -> Self {
        Self {
            peer,
            request_id,
            request,
        }
    }
}

/// Peer context.
struct PeerContext {
    /// Active requests.
    active: HashSet<RequestId>,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new() -> Self {
        Self {
            active: HashSet::new(),
        }
    }
}

/// Request-response protocol.
pub(crate) struct RequestResponseProtocol {
    /// Transport service.
    service: TransportService,

    /// Connected peers.
    peers: HashMap<PeerId, PeerContext>,

    /// Pending outbound substreams, mapped from `SubstreamId` to `RequestId`.
    pending_outbound: HashMap<SubstreamId, RequestContext>,

    /// Pending outbound responses.
    pending_outbound_responses: HashMap<RequestId, Box<dyn Substream>>,

    /// Pending inbound responses.
    pending_inbound: FuturesUnordered<BoxFuture<'static, PendingRequest>>,

    /// Pending dials for outbound requests.
    pending_dials: HashMap<PeerId, RequestContext>,

    /// TX channel for sending events to the user protocol.
    event_tx: Sender<RequestResponseEvent>,

    /// RX channel for receive commands from the `RequestResponseHandle`.
    command_rx: Receiver<RequestResponseCommand>,

    /// Next request ID.
    ///
    /// Inbound requests are assigned an ephemeral ID TODO: finish
    next_request_id: Arc<AtomicUsize>,
}

impl RequestResponseProtocol {
    /// Create new [`RequestResponseProtocol`].
    pub(crate) fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            peers: HashMap::new(),
            next_request_id: config.next_request_id,
            event_tx: config.event_tx,
            command_rx: config.command_rx,
            pending_dials: HashMap::new(),
            pending_outbound: HashMap::new(),
            pending_outbound_responses: HashMap::new(),
            pending_inbound: FuturesUnordered::new(),
        }
    }

    /// Get next ephemeral request ID.
    fn next_request_id(&mut self) -> RequestId {
        RequestId::from(self.next_request_id.fetch_add(1usize, Ordering::Relaxed))
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        let Entry::Vacant(entry) = self.peers.entry(peer) else {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                "state mismatch: peer already exists"
            );
            debug_assert!(false);
            return Err(Error::PeerAlreadyExists(peer))
        };

        match self.pending_dials.remove(&peer) {
            None => {
                entry.insert(PeerContext::new());
                Ok(())
            }
            Some(context) => match self.service.open_substream(peer).await {
                Ok(substream_id) => {
                    self.pending_outbound.insert(
                        substream_id,
                        RequestContext::new(peer, context.request_id, context.request),
                    );
                    Ok(())
                }
                Err(error) => {
                    tracing::debug!(target: LOG_TARGET, ?peer, request_id = ?context.request_id, ?error, "failed to open substream");
                    self.report_request_failure(
                        peer,
                        context.request_id,
                        RequestResponseError::Rejected,
                    )
                    .await
                }
            },
        }
    }

    /// Connection closed to remote peer.
    async fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        let Some(context) = self.peers.remove(&peer) else {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                "state mismatch: peer doesn't exist"
            );
            debug_assert!(false);
            return;
        };

        for request_id in context.active {
            let _ = self
                .event_tx
                .send(RequestResponseEvent::RequestFailed {
                    peer,
                    request_id,
                    error: RequestResponseError::Rejected,
                })
                .await;
        }
    }

    /// Local node opened a substream to remote node.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        let Some(RequestContext {
            request_id,
            request,
            ..
        }) = self.pending_outbound.remove(&substream_id)
        else {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                ?substream_id,
                "pending outbound request does not exist"
            );
            debug_assert!(false);

            return Err(Error::InvalidState);
        };

        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?substream_id,
            ?request_id,
            "substream opened, send request",
        );

        match substream.send(request.into()).await {
            Ok(_) => {
                self.pending_inbound.push(Box::pin(async move {
                    match timeout(REQUEST_TIMEOUT, substream.next()).await {
                        Err(_) => (peer, request_id, Err(RequestResponseError::Timeout)),
                        Ok(Some(Ok(response))) => (peer, request_id, Ok(response.freeze().into())),
                        _ => (peer, request_id, Err(RequestResponseError::Rejected)),
                    }
                }));

                Ok(())
            }
            Err(Error::IoError(ErrorKind::PermissionDenied)) => {
                tracing::warn!(target: LOG_TARGET, "tried to send too large request");

                self.event_tx
                    .send(RequestResponseEvent::RequestFailed {
                        peer,
                        request_id,
                        error: RequestResponseError::TooLargePayload,
                    })
                    .await
                    .map_err(From::from)
            }
            Err(_error) => self
                .event_tx
                .send(RequestResponseEvent::RequestFailed {
                    peer,
                    request_id,
                    error: RequestResponseError::NotConnected,
                })
                .await
                .map_err(From::from),
        }
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        fallback: Option<ProtocolName>,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

        // TODO: potential attack vector
        let request = substream
            .next()
            .await
            .ok_or(Error::SubstreamError(SubstreamError::ReadFailure(None)))??
            .freeze()
            .to_vec();

        // allocate ephemeral id for the inbound request and return it to the user protocol.
        // when user responds to the request, this is used to associate the response with the
        // correct substream.
        let request_id = self.next_request_id();

        match self
            .event_tx
            .send(RequestResponseEvent::RequestReceived {
                peer,
                fallback,
                request_id,
                request,
            })
            .await
            .map_err(From::from)
        {
            Ok(_) => {
                self.pending_outbound_responses.insert(request_id, substream);
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    async fn on_dial_failure(&mut self, peer: PeerId) {
        if let Some(context) = self.pending_dials.remove(&peer) {
            tracing::debug!(target: LOG_TARGET, ?peer, "failed to dial peer");

            let _ = self
                .report_request_failure(peer, context.request_id, RequestResponseError::Rejected)
                .await;
        }
    }

    /// Failed to open substream to remote peer.
    async fn on_substream_open_failure(
        &mut self,
        substream: SubstreamId,
        error: Error,
    ) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            ?substream,
            ?error,
            "failed to open substream"
        );

        let Some(RequestContext {
            request_id, peer, ..
        }) = self.pending_outbound.remove(&substream)
        else {
            tracing::error!(
                target: LOG_TARGET,
                ?substream,
                "pending outbound request does not exist"
            );
            debug_assert!(false);

            return Err(Error::InvalidState);
        };

        self.event_tx
            .send(RequestResponseEvent::RequestFailed {
                peer,
                request_id,
                error: RequestResponseError::Rejected,
            })
            .await
            .map_err(From::from)
    }

    /// Report request send failure to user.
    async fn report_request_failure(
        &mut self,
        peer: PeerId,
        request_id: RequestId,
        error: RequestResponseError,
    ) -> crate::Result<()> {
        self.event_tx
            .send(RequestResponseEvent::RequestFailed {
                peer,
                request_id,
                error,
            })
            .await
            .map_err(From::from)
    }

    /// Send request to remote peer.
    async fn on_send_request(
        &mut self,
        peer: PeerId,
        request_id: RequestId,
        request: Vec<u8>,
        dial_options: DialOptions,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?request_id,
            ?dial_options,
            "send request to remote peer"
        );

        let Some(context) = self.peers.get_mut(&peer) else {
            match dial_options {
                DialOptions::Reject => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?request_id,
                        ?dial_options,
                        "peer not connected and should not dial"
                    );
                    return self.report_request_failure(peer, request_id, RequestResponseError::NotConnected).await;
                }
                DialOptions::Dial => match self.service.dial(&peer).await {
                    Ok(_) => {
                        self.pending_dials.insert(peer, RequestContext::new(peer, request_id, request));
                        return Ok(())
                    }
                    Err(error) => {
                        tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to dial peer");
                        return self.report_request_failure(peer, request_id, RequestResponseError::Rejected).await;
                    }
                }
            }
        };

        if !context.active.insert(request_id) {
            tracing::error!(
                target: LOG_TARGET,
                ?request_id,
                "state mismatch: reused request ID"
            );
            debug_assert!(false);
        }

        // open substream and push it pending outbound substreams
        // once the substream is opened, send the request.
        match self.service.open_substream(peer).await {
            Ok(substream_id) => {
                self.pending_outbound
                    .insert(substream_id, RequestContext::new(peer, request_id, request));
                Ok(())
            }
            Err(error) => {
                tracing::debug!(target: LOG_TARGET, ?peer, ?request_id, ?error, "failed to open substream");
                self.report_request_failure(peer, request_id, RequestResponseError::Rejected)
                    .await
            }
        }
    }

    /// Send response to remote peer.
    async fn on_send_response(
        &mut self,
        request_id: RequestId,
        response: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?request_id,
            ?response,
            "send response to remote peer"
        );

        match self.pending_outbound_responses.remove(&request_id) {
            Some(mut substream) => match substream.send(response.into()).await {
                Ok(()) => Ok(()),
                Err(error) => {
                    tracing::trace!(target: LOG_TARGET, ?request_id, ?error, "failed to send response");
                    let _ = substream.close().await;
                    Ok(())
                }
            },
            None => return Err(Error::Other(format!("pending request doesn't exist"))),
        }
    }

    /// Handle substream event.
    async fn on_substream_event(
        &mut self,
        peer: PeerId,
        request_id: RequestId,
        message: Result<Vec<u8>, RequestResponseError>,
    ) -> crate::Result<()> {
        let event = match message {
            Ok(response) => RequestResponseEvent::ResponseReceived {
                peer,
                request_id,
                response,
            },
            Err(error) => RequestResponseEvent::RequestFailed {
                peer,
                request_id,
                error,
            },
        };

        self.event_tx.send(event).await.map_err(From::from)
    }

    /// Start [`RequestResponseProtocol`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting request-response event loop");

        loop {
            tokio::select! {
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
                        self.on_connection_closed(peer).await;
                    }
                    Some(TransportEvent::SubstreamOpened {
                        peer,
                        substream,
                        direction,
                        fallback,
                        ..
                    }) => match direction {
                        Direction::Inbound => {
                            if let Err(error) = self.on_inbound_substream(peer, fallback, substream).await {
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
                                .on_outbound_substream(peer, substream_id, substream)
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
                        if let Err(error) = self.on_substream_open_failure(substream, error).await {
                            tracing::warn!(target: LOG_TARGET, ?error, "failed to handle substream open failure");
                        }
                    }
                    Some(TransportEvent::DialFailure { peer, .. }) => self.on_dial_failure(peer).await,
                    None => return,
                },
                command = self.command_rx.recv() => match command {
                    None => {
                        tracing::debug!(target: LOG_TARGET, "user protocol has exited, exiting");
                        return
                    }
                    Some(command) => match command {
                        RequestResponseCommand::SendRequest { peer, request_id, request, dial_options } => {
                            if let Err(error) = self.on_send_request(peer, request_id, request, dial_options).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?request_id,
                                    ?error,
                                    "failed to send request"
                                );
                            }
                        }
                        RequestResponseCommand::SendResponse { request_id, response } => {
                            if let Err(error) = self.on_send_response(request_id, response).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?request_id,
                                    ?error,
                                    "failed to send response"
                                );
                            }
                        },
                        RequestResponseCommand::RejectRequest { request_id } => {
                            tracing::trace!(target: LOG_TARGET, ?request_id, "reject request");

                            if let Some(mut substream) = self.pending_outbound_responses.remove(&request_id) {
                                let _ = substream.close().await;
                            }
                        }
                    }
                },
                event = self.pending_inbound.select_next_some(), if !self.pending_inbound.is_empty() => {
                    let (peer, request_id, event) = event;

                    if let Err(error) = self.on_substream_event(peer, request_id, event).await {
                        tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to handle substream event");
                    }
                }
            }
        }
    }
}
