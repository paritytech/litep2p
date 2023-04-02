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

// TODO: implement timeouts
// TODO: move this to its own module

use crate::{
    peer_id::PeerId,
    protocol::TransportCommand,
    transport::{Connection, Direction, TransportEvent},
    DEFAULT_CHANNEL_SIZE,
};

use futures::{stream::FuturesUnordered, AsyncReadExt, AsyncWriteExt, StreamExt};
use tokio::sync::{mpsc, oneshot};

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
};

/// Logging target for the file.
const LOG_TARGET: &str = "request-response";

/// Logging target for the file for logging binary messages.
const LOG_TARGET_MSG: &str = "request-response::msg";

/// Unique ID for a request.
pub type RequestId = usize;

pub enum RequestResponseError {
    /// Request timed out.
    Timeout,

    /// Remote rejected the request by closing the substream.
    Rejected,

    /// Sender is sending too many requests to peer and the limit
    /// on the number of substreams has been exhausted.
    TooManyRequests,
}

/// Events emitted by [`RequestResponseService`].
pub enum RequestResponseEvent {
    /// Request received from remote peer.
    RequestReceived {
        /// Remote peer ID.
        peer: PeerId,

        /// Request ID.
        request_id: RequestId,

        /// Received request.
        request: Vec<u8>,
    },

    /// Response received from remote peer.
    ResponseReceived {
        /// ID for the request this is a response to.
        request: RequestId,

        /// Response.
        response: Vec<u8>,
    },

    /// Request failed.
    RequestFailure {
        /// Request that failed.
        request: RequestId,

        /// Request error.
        error: RequestResponseError,
    },
}

/// Type representing a pending outbound request.
// TODO: rename
type PendingRequest = Pin<Box<dyn Future<Output = crate::Result<(RequestId, Vec<u8>)>> + Send>>;

/// Type representing a pending inbound request.
type PendingInboundRequest =
    Pin<Box<dyn Future<Output = crate::Result<(PeerId, Vec<u8>, Box<dyn Connection>)>> + Send>>;

pub struct RequestResponseProtocolConfig {
    /// Protocol.
    pub(crate) protocol: String,

    /// TX channel for sending `TransportEvent`s.
    pub(crate) tx: mpsc::Sender<TransportEvent>,

    /// RX channel for receiving [`TransportCommand`]s from `RequestResponseService`.
    pub(crate) rx: mpsc::Receiver<TransportCommand>,
}

impl RequestResponseProtocolConfig {
    /// Create new [`RequestResponseProtocolConfig`] and return [`RequestResponseService`]
    /// which can be used by the protocol to interact with this request-response protocol.
    pub fn new(
        protocol: String,
        channel_size: Option<usize>,
    ) -> (RequestResponseProtocolConfig, RequestResponseService) {
        tracing::debug!(
            target: LOG_TARGET,
            ?protocol,
            ?channel_size,
            "initialize new request-response protocol"
        );

        let (command_tx, command_rx) = mpsc::channel(channel_size.unwrap_or(DEFAULT_CHANNEL_SIZE));
        let (event_tx, event_rx) = mpsc::channel(channel_size.unwrap_or(DEFAULT_CHANNEL_SIZE));

        (
            Self {
                protocol: protocol.clone(),
                tx: command_tx,
                rx: event_rx,
            },
            RequestResponseService::new(protocol, event_tx, command_rx),
        )
    }
}

enum RequestState {
    OpenInitiated {
        request_id: RequestId,
        request: Vec<u8>,
    },
}

pub struct RequestResponseService {
    /// Next available request ID.
    next_request_id: RequestId,

    /// Next available request ID for associating inbound requests with outbound responses.
    next_inbound_request_id: RequestId,

    /// Protocol.
    protocol: String,

    /// Pending requests.
    pending_requests: FuturesUnordered<PendingRequest>,

    /// Pending inbound requests.
    pending_inbound_requests: FuturesUnordered<PendingInboundRequest>,

    /// TX channel for sending [`TransportCommand`]s to `Litep2p`.
    tx: mpsc::Sender<TransportCommand>,

    /// RX channel for receiving `TransportEvent`s from `Litep2p`.
    rx: mpsc::Receiver<TransportEvent>,

    /// Pending requests
    pending_requests2: VecDeque<RequestState>,

    /// Pending outbound responses.
    pending_responses: HashMap<RequestId, Box<dyn Connection>>,
}

impl RequestResponseService {
    /// Create new [`RequestResponseService`].
    fn new(
        protocol: String,
        tx: mpsc::Sender<TransportCommand>,
        rx: mpsc::Receiver<TransportEvent>,
    ) -> Self {
        Self {
            next_request_id: 0,
            next_inbound_request_id: 0,
            protocol,
            tx,
            rx,
            pending_requests: FuturesUnordered::new(),
            pending_inbound_requests: FuturesUnordered::new(),
            pending_requests2: VecDeque::new(),
            pending_responses: HashMap::new(),
        }
    }

    /// Get the next request ID.
    fn next_request_id(&mut self) -> RequestId {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        request_id
    }

    /// Get the next request ID for an inbound request.
    fn next_inbound_request_id(&mut self) -> RequestId {
        let request_id = self.next_inbound_request_id;
        self.next_inbound_request_id += 1;
        request_id
    }

    /// Attempt to send request to remote peer.
    ///
    /// This function only initiates the request and it is completed in the background.
    /// The returned [`RequestId`] can be used to associate incoming responses to sent requests
    pub async fn send_request(
        &mut self,
        peer: PeerId,
        request: Vec<u8>,
    ) -> crate::Result<RequestId> {
        let request_id = self.next_request_id();

        tracing::trace!(target: LOG_TARGET, ?peer, ?request_id, "send request");

        // initiate request by opening a substream.
        //
        // once the substream has been opened, it's reported to [`RequestResponseService`]
        // which then sends the request over the substream and starts a timer.
        self.pending_requests2
            .push_back(RequestState::OpenInitiated {
                request_id,
                request,
            });

        self.tx
            .send(TransportCommand::OpenSubstream {
                protocol: self.protocol.clone(),
                peer,
            })
            .await?;

        Ok(request_id)
    }

    /// Send response.
    pub async fn send_response(
        &mut self,
        request: RequestId,
        response: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET_MSG, ?request, ?response, "send response");

        match self.pending_responses.remove(&request) {
            Some(mut substream) => substream
                .write(&response)
                .await
                .map(|_| ())
                .map_err(From::from),
            None => todo!(),
        }
    }

    /// Handle inbound substream.
    async fn handle_inbound_substream(&mut self, peer: PeerId, mut substream: Box<dyn Connection>) {
        tracing::trace!(target: LOG_TARGET, ?peer, "inbound substream received");

        self.pending_inbound_requests.push(Box::pin(async move {
            let mut buffer = vec![0u8; 2048];
            let nread = substream.read(&mut buffer).await?;
            Ok((peer, buffer[..nread].to_vec(), substream))
        }));
    }

    /// Handle outbound substream.
    // TODO: use span
    async fn handle_outbound_substream(
        &mut self,
        peer: PeerId,
        mut substream: Box<dyn Connection>,
    ) {
        tracing::trace!(target: LOG_TARGET, ?peer, "outbound substream received");

        match self.pending_requests2.pop_front() {
            Some(state) => match state {
                RequestState::OpenInitiated {
                    request_id,
                    request,
                } => {
                    tracing::trace!(target: LOG_TARGET_MSG, ?peer, ?request, "send request");

                    self.pending_requests.push(Box::pin(async move {
                        // send request
                        substream.write(&request).await?;

                        // send response
                        let mut buffer = vec![0u8; 2048];
                        let nread = substream.read(&mut buffer).await?;
                        Ok((request_id, buffer[..nread].to_vec()))
                    }));
                }
            },
            None => {
                tracing::debug!(
                    target: LOG_TARGET,
                    "opened substream is not part of any pending request"
                );
            }
        }
    }

    /// Poll next event from the stream.
    pub async fn next_event(&mut self) -> Option<RequestResponseEvent> {
        loop {
            tokio::select! {
                event = self.rx.recv() => match event? {
                    TransportEvent::SubstreamOpened(_, peer, direction, substream) => match direction {
                        Direction::Inbound => self.handle_inbound_substream(peer, substream).await,
                        Direction::Outbound => self.handle_outbound_substream(peer, substream).await,
                    },
                    event => tracing::debug!(target: LOG_TARGET, ?event, "ignoring `TransportEvent`"),
                },
                result = self.pending_requests.select_next_some(), if !self.pending_requests.is_empty() => {
                    match result {
                        Ok((request, response)) => {
                            return Some(RequestResponseEvent::ResponseReceived { request, response })
                        }
                        Err(err) => {
                            todo!();
                        }
                    }
                }
                result = self.pending_inbound_requests.select_next_some(), if !self.pending_inbound_requests.is_empty() => {
                    match result {
                        Ok((peer, request, substream)) => {
                            tracing::trace!(
                                target: LOG_TARGET_MSG,
                                ?peer,
                                ?request,
                                "request received",
                            );
                            let request_id = self.next_inbound_request_id();
                            self.pending_responses.insert(request_id, substream);

                            return Some(RequestResponseEvent::RequestReceived {
                                peer,
                                request_id,
                                request,
                            });
                        }
                        Err(err) => {
                            todo!();
                        }
                    }
                }
            }
        }
    }
}
