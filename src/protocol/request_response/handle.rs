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

use crate::{peer_id::PeerId, types::RequestId};

use tokio::sync::mpsc::{Receiver, Sender};

/// Logging target for the file.
const LOG_TARGET: &str = "request-response::handle";

/// Request-response error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestResponseError {
    /// Request was rejected.
    Rejected,

    /// Request timed out.
    Timeout,

    /// Litep2p isn't connected to the peer.
    NotConnected,
}

/// Request-response events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestResponseEvent {
    /// Request received from remote
    RequestReceived {
        /// Peer Id.
        peer: PeerId,

        /// Request ID.
        request_id: RequestId,

        /// Received request.
        request: Vec<u8>,
    },

    /// Response received.
    ResponseReceived {
        /// Peer Id.
        peer: PeerId,

        /// Request ID.
        request_id: RequestId,

        /// Received request.
        response: Vec<u8>,
    },

    /// Request failed.
    RequestFailed {
        /// Peer Id.
        peer: PeerId,

        /// Request ID.
        request_id: RequestId,

        /// Request-response error.
        error: RequestResponseError,
    },
}

/// Request-response commands.
pub(crate) enum RequestResponseCommand {
    /// Send request to remote peer.
    SendRequest {
        /// Peer ID.
        peer: PeerId,

        /// Request ID.
        ///
        /// When a response is received or the request fails, the event contains this ID that
        /// the user protocol can associate with the correct request.
        ///
        /// If the user protocol only has one active request per peer, this ID can be safely discarded.
        request_id: RequestId,

        /// Request.
        request: Vec<u8>,
    },

    /// Send response.
    SendResponse {
        /// Request ID.
        ///
        /// This is the request ID that was received in [`RequestResponseEvent::RequestReceived`].
        request_id: RequestId,

        /// Response.
        response: Vec<u8>,
    },

    /// Reject request.
    RejectRequest {
        /// Request ID.
        request_id: RequestId,
    },
}

/// Handle given to the user protocol which allows it to interact with the request-response protocol.
pub struct RequestResponseHandle {
    /// TX channel for sending commands to the request-response protocol.
    event_rx: Receiver<RequestResponseEvent>,

    /// RX channel for receiving events from the request-response protocol.
    command_tx: Sender<RequestResponseCommand>,

    /// Next ephemeral request ID.
    next_request_id: RequestId,
}

impl RequestResponseHandle {
    /// Create new [`RequestResponseHandle`].
    pub(crate) fn new(
        event_rx: Receiver<RequestResponseEvent>,
        command_tx: Sender<RequestResponseCommand>,
    ) -> Self {
        Self {
            event_rx,
            command_tx,
            next_request_id: 0usize,
        }
    }

    /// Get next ephemeral request ID.
    // TODO: make this a helper of `RequestId`.
    fn next_request_id(&mut self) -> RequestId {
        let request_id = self.next_request_id;
        self.next_request_id += 1;

        request_id
    }

    /// Reject request.
    pub async fn reject_request(&mut self, request_id: RequestId) {
        tracing::trace!(target: LOG_TARGET, ?request_id, "reject request");

        let _ = self
            .command_tx
            .send(RequestResponseCommand::RejectRequest { request_id })
            .await;
    }

    /// Send request to remote peer.
    pub async fn send_request(
        &mut self,
        peer: PeerId,
        request: Vec<u8>,
    ) -> crate::Result<RequestId> {
        tracing::trace!(target: LOG_TARGET, ?peer, "send request to peer");

        let request_id = self.next_request_id();
        self.command_tx
            .send(RequestResponseCommand::SendRequest {
                peer,
                request_id,
                request,
            })
            .await
            .map(|_| request_id)
            .map_err(From::from)
    }

    /// Send response to remote peer.
    pub async fn send_response(
        &mut self,
        request_id: RequestId,
        response: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?request_id, "send response to peer");

        self.command_tx
            .send(RequestResponseCommand::SendResponse {
                request_id,
                response,
            })
            .await
            .map_err(From::from)
    }

    /// Poll next event from the request-response protocol.
    pub async fn next_event(&mut self) -> Option<RequestResponseEvent> {
        self.event_rx.recv().await
    }
}
