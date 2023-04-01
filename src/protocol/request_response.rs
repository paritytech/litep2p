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

use crate::{peer_id::PeerId, transport::TransportEvent, DEFAULT_CHANNEL_SIZE};

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
}

pub struct RequestResponseService {}

impl RequestResponseService {
    fn new() -> Self {
        Self {}
    }

    /// Poll next event from the stream.
    pub fn next_event(&mut self) -> Option<RequestResponseEvent> {
        todo!();
    }
}

/// Generic request-response protocol.
pub struct RequestResponseProtocol {
    /// Name of the request-response protocol.
    protocol: String,
}

impl RequestResponseProtocol {
    /// Create new [`RequestResponseProtocol`].
    pub fn new(protocol: String, channel_size: Option<usize>) -> Self {
        // let (tx, rx) = mpsc::channel(channel_size.unwrap_or(DEFAULT_CHANNEL_SIZE));
        Self { protocol }
    }

    /// Start event loop for a [`RequestResponseProtocol`].
    pub(crate) async fn start(self) {
        todo!();
    }
}
