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
    codec::ProtocolCodec,
    peer_id::PeerId,
    types::{protocol::ProtocolName, RequestId},
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};

/// Request-response error.
pub enum RequestResponseError {
    /// Request was rejected.
    Rejected,

    /// Request timed out.
    Timeout,
}

/// Request-response events.
pub enum RequestReponseEvent {
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

        /// Received request.
        request: Vec<u8>,
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

pub enum InnerRequestResponseEvent {}
pub enum RequestResponseCommand {}

pub struct RequestResponseHandle {
    _event_rx: Receiver<InnerRequestResponseEvent>,
    _command_tx: Sender<RequestResponseCommand>,
}

impl RequestResponseHandle {
    pub fn new(
        _event_rx: Receiver<InnerRequestResponseEvent>,
        _command_tx: Sender<RequestResponseCommand>,
    ) -> Self {
        Self {
            _event_rx,
            _command_tx,
        }
    }

    /// Send request to remote peer.
    pub async fn send_request(
        &mut self,
        _peer: PeerId,
        _request: Vec<u8>,
    ) -> crate::Result<RequestId> {
        todo!();
    }

    /// Send response to remote peer.
    pub async fn send_response(
        &mut self,
        _request_id: RequestId,
        _response: Vec<u8>,
    ) -> crate::Result<()> {
        todo!();
    }

    /// Poll next event from the request-response protocol.
    pub async fn next_event(&mut self) -> Option<RequestReponseEvent> {
        todo!();
    }
}

/// Request-response configuration.
#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    protocol_name: ProtocolName,

    /// Codec used by the protocol.
    pub(crate) codec: ProtocolCodec,

    /// Maximum slots allocated for inbound requests.
    pub(crate) _max_slots: usize,

    /// TX channel for sending events to the user protocol.
    pub(crate) _event_tx: Sender<InnerRequestResponseEvent>,

    /// RX channel for receiving commands from the user protocol.
    pub(crate) _command_rx: Receiver<RequestResponseCommand>,
}

impl Config {
    /// Create new [`Config`].
    pub fn new(protocol_name: ProtocolName, _max_slots: usize) -> (Self, RequestResponseHandle) {
        let (_event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let (command_tx, _command_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let handle = RequestResponseHandle::new(event_rx, command_tx);

        (
            Self {
                protocol_name,
                _max_slots,
                _event_tx,
                _command_rx,
                codec: ProtocolCodec::UnsignedVarint,
            },
            handle,
        )
    }

    /// Get protocol name.
    pub(crate) fn protocol_name(&self) -> &ProtocolName {
        &self.protocol_name
    }
}
