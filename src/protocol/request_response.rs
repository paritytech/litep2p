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

// TODO: what is needed:
//      - way to receive inbound substream (Receiver<TransportEvent>) (how to give this?)
//      - way to poll substreams (FuturesUnorderd)
//      - way to open substreams (Sender<TransportCommand>) (how to give this) (when initialized)
//      - way send requests on opened substreams (substream.write())
//      - way to send responses on opened substreams (substream.write())
//      - way to send requests/responses to user (Sender<RequestResponseEvent>)

use crate::{
    peer_id::PeerId, protocol::TransportCommand, transport::TransportEvent, DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::{mpsc, oneshot};

pub enum RequestResponseEvent {
    RequestReceived {
        /// Remote peer ID.
        peer: PeerId,

        /// Received request.
        request: Vec<u8>,

        /// `oneshot::Sender` for sending the response.
        tx: oneshot::Sender<Vec<u8>>,
    },

    /// Response received from remote peer.
    ResponseReceived {
        /// ID for the request this is a response to.
        request_id: RequestId,

        /// Response.
        response: Vec<u8>,
    },
}

/// Unique ID for a request.
pub type RequestId = usize;

pub struct RequestResponseProtocolConfig {}

impl RequestResponseProtocolConfig {
    /// Create new [`RequestResponseProtocolConfig`] and return [`RequestResponseService`]
    /// which can be used by the protocol to interact with this request-response protocol.
    pub fn new(
        protocol: String,
        channel_size: Option<usize>,
    ) -> (RequestResponseProtocolConfig, RequestResponseService) {
        todo!();
    }
}

pub struct RequestResponseService {
    /// Next available request ID.
    next_request_id: RequestId,

    /// TX channel for sending commands to `Litep2p`.
    tx: mpsc::Sender<TransportCommand>,
}

impl RequestResponseService {
    fn new() -> Self {
        Self { next_request_id: 0 }
    }

    /// Get the next request ID.
    fn next_request_id(&mut self) -> RequestId {
        let request_id = self.next_request_id;
        self.next_request_id += 1;
        request_id
    }

    /// Attempt to send request to remote peer.
    ///
    /// This function only initiates the request and it is completed in the background.
    /// The returned [`RequestId`] can be used to associate incoming responses to sent requests
    pub fn send_request(&mut self, peer: PeerId, request: Vec<u8>) -> crate::Result<RequestId> {
        let request_id = self.next_request_id();

        Ok(request_id)
    }

    /// Send response.
    pub fn send_response(&mut self, request: RequestId, response: Vec<u8>) -> crate::Result<()> {
        Ok(())
    }

    /// Poll next event from the stream.
    pub fn next_event(&mut self) -> Option<RequestResponseEvent> {
        todo!();
    }
}
