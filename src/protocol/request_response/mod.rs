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
    protocol::request_response::types::Config,
    protocol::{
        request_response::types::{
            RequestResponseCommand, RequestResponseError, RequestResponseEvent,
        },
        Direction, Transport, TransportEvent, TransportService,
    },
    substream::Substream,
    types::{RequestId, SubstreamId},
};

use futures::{future::BoxFuture, stream::FuturesUnordered, SinkExt, StreamExt};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    time::timeout,
};

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    time::Duration,
};

pub mod types;

/// Logging target for the file.
const LOG_TARGET: &str = "request-response::protocol";

/// Default request timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Pending request.
pub type PendingRequest = (PeerId, RequestId, Result<Vec<u8>, RequestResponseError>);

/// Request context.
struct RequestContext {
    /// Request ID.
    request_id: RequestId,

    /// Request.
    request: Vec<u8>,
}

impl RequestContext {
    /// Create new [`RequestContext`].
    fn new(request_id: RequestId, request: Vec<u8>) -> Self {
        Self {
            request_id,
            request,
        }
    }
}

/// Peer context.
struct PeerContext {
    /// Connection service.
    service: (),

    /// Active requests.
    active: HashSet<RequestId>,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new(service: ()) -> Self {
        Self {
            service,
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

    /// TX channel for sending events to the user protocol.
    event_tx: Sender<RequestResponseEvent>,

    /// RX channel for receive commands from the `RequestResponseHandle`.
    command_rx: Receiver<RequestResponseCommand>,

    /// Next request ID.
    ///
    /// Inbound requests are assigned an ephemeral ID TODO: finish
    next_request_id: RequestId,
}

impl RequestResponseProtocol {
    /// Create new [`RequestResponseProtocol`].
    pub(crate) fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            peers: HashMap::new(),
            next_request_id: 0usize,
            event_tx: config.event_tx,
            command_rx: config.command_rx,
            pending_outbound: HashMap::new(),
            pending_outbound_responses: HashMap::new(),
            pending_inbound: FuturesUnordered::new(),
        }
    }

    /// Get next ephemeral request ID.
    // TODO: make this a helper of `RequestId`.
    fn next_request_id(&mut self) -> RequestId {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        request_id
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        match self.peers.entry(peer) {
            Entry::Vacant(entry) => {
                entry.insert(PeerContext::new(()));
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
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        if let None = self.peers.remove(&peer) {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                "state mismatch: peer doesn't exist"
            );
            debug_assert!(false);
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
        }) = self.pending_outbound.remove(&substream_id)
        else {
            tracing::error!(
                target: LOG_TARGET,
                ?peer,
                ?substream_id,
                "pending outbound request does not exist"
            );
            debug_assert!(false);

            return Err(Error::Other(format!(
                "pending outbound request does not exist"
            )));
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
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

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
                request_id,
                request,
            })
            .await
            .map_err(From::from)
        {
            Ok(_) => {
                self.pending_outbound_responses
                    .insert(request_id, substream);
                Ok(())
            }
            Err(error) => Err(error),
        }
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

    /// Send request to remote peer.
    async fn on_send_request(
        &mut self,
        peer: PeerId,
        request_id: RequestId,
        request: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?request_id,
            "send request to remote peer"
        );

        let Some(context) = self.peers.get_mut(&peer) else {
            let _ = self
                .event_tx
                .send(RequestResponseEvent::RequestFailed {
                    peer,
                    request_id,
                    error: RequestResponseError::NotConnected,
                })
                .await;

            return Err(Error::PeerDoesntExist(peer));
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
        let substream_id = self.service.open_substream(peer).await?;
        self.pending_outbound
            .insert(substream_id, RequestContext::new(request_id, request));

        Ok(())
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
            Some(mut substream) => substream.send(response.into()).await.map_err(From::from),
            None => {
                return Err(Error::Other(format!("pending request doesn't exist")));
            }
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
                        self.on_connection_closed(peer);
                    }
                    Some(TransportEvent::SubstreamOpened {
                        peer,
                        substream,
                        direction,
                        ..
                    }) => match direction {
                        Direction::Inbound => {
                            if let Err(error) = self.on_inbound_substream(peer, substream).await {
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
                        self.on_substream_open_failure(substream, error);
                    }
                    Some(TransportEvent::DialFailure { .. }) => todo!(),
                    None => return,
                },
                command = self.command_rx.recv() => match command {
                    None => {
                        tracing::debug!(target: LOG_TARGET, "user protocol has exited, exiting");
                        return
                    }
                    Some(command) => match command {
                        RequestResponseCommand::SendRequest { peer, request_id, request } => {
                            if let Err(error) = self.on_send_request(peer, request_id, request).await {
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
