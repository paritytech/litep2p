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
    protocol::libp2p::bitswap::{BitswapEvent, BitswapHandle},
    types::protocol::ProtocolName,
};

use tokio::sync::mpsc::{channel, Sender};

/// IPFS Bitswap protocol name as a string.
pub const PROTOCOL_NAME: &str = "/ipfs/bitswap/1.2.0";

/// Maximum Size for `/ipfs/bitswap/1.2.0` payloads.
const MAX_PAYLOAD_SIZE: usize = 2_097_152;

/// Bitswap configuration.
pub struct BitswapConfig {
    /// TX channel for sending events to the user protocol.
    pub(super) event_tx: Sender<BitswapEvent>,

    /// Protocol name.
    pub(super) protocol_name: ProtocolName,

    /// Protocol codec.
    pub(super) protocol_codec: ProtocolCodec,
}

impl BitswapConfig {
    /// Create new [`BitswapConfig`].
    pub fn new() -> (Self, BitswapHandle) {
        let (event_tx, event_rx) = channel(256);

        (
            Self {
                event_tx,
                protocol_name: ProtocolName::from(PROTOCOL_NAME),
                protocol_codec: ProtocolCodec::UnsignedVarint(Some(MAX_PAYLOAD_SIZE)),
            },
            BitswapHandle::new(event_rx),
        )
    }
}
