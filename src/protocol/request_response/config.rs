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
    protocol::request_response::handle::{
        RequestResponseCommand, RequestResponseEvent, RequestResponseHandle,
    },
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};

/// Request-response configuration.
#[derive(Debug)]
pub struct RequestResponseConfig {
    /// Protocol name.
    pub(crate) protocol_name: ProtocolName,

    /// Codec used by the protocol.
    pub(crate) codec: ProtocolCodec,

    /// Maximum slots allocated for inbound requests.
    pub(crate) _max_slots: usize,

    /// TX channel for sending events to the user protocol.
    pub(crate) event_tx: Sender<RequestResponseEvent>,

    /// RX channel for receiving commands from the user protocol.
    pub(crate) command_rx: Receiver<RequestResponseCommand>,
}

impl RequestResponseConfig {
    /// Create new [`Config`].
    pub fn new(protocol_name: ProtocolName, _max_slots: usize) -> (Self, RequestResponseHandle) {
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let (command_tx, command_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let handle = RequestResponseHandle::new(event_rx, command_tx);

        (
            Self {
                protocol_name,
                _max_slots,
                event_tx,
                command_rx,
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
