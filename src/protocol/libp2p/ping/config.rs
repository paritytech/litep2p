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
    codec::ProtocolCodec, protocol::libp2p::ping::PingEvent, types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use futures::Stream;
use tokio::sync::mpsc::{channel, Sender};
use tokio_stream::wrappers::ReceiverStream;

/// IPFS Ping protocol name as a string.
pub const PROTOCOL_NAME: &str = "/ipfs/ping/1.0.0";

/// Size for `/ipfs/ping/1.0.0` payloads.
const PING_PAYLOAD_SIZE: usize = 32;

/// Maximum PING failures.
const MAX_FAILURES: usize = 3;

/// Ping configuration.
#[derive(Debug)]
pub struct PingConfig {
    /// Protocol name.
    pub(crate) protocol: ProtocolName,

    /// Codec used by the protocol.
    pub(crate) codec: ProtocolCodec,

    /// Maximum failures before the peer is considered unreachable.
    pub(crate) max_failures: usize,

    /// Should the connection be kept alive using PING.
    pub(crate) keep_alive: bool,

    /// TX channel for sending events to the user protocol.
    pub(crate) tx_event: Sender<PingEvent>,
}

impl PingConfig {
    /// Create new [`PingConfig`] with default values.
    ///
    /// Returns a config that is given to `Litep2pConfig` and an event stream for ping events.
    pub fn default() -> (Self, Box<dyn Stream<Item = PingEvent> + Send + Unpin>) {
        let (tx_event, rx_event) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                tx_event,
                keep_alive: false,
                max_failures: MAX_FAILURES,
                protocol: ProtocolName::from(PROTOCOL_NAME),
                codec: ProtocolCodec::Identity(PING_PAYLOAD_SIZE),
            },
            Box::new(ReceiverStream::new(rx_event)),
        )
    }
}

/// PING configuration builder.
pub struct PingConfigBuilder {
    /// Protocol name.
    protocol: ProtocolName,

    /// Codec used by the protocol.
    codec: ProtocolCodec,

    /// Maximum failures before the peer is considered unreachable.
    max_failures: usize,

    /// Should the connection be kept alive using PING.
    keep_alive: bool,
}

impl PingConfigBuilder {
    /// Create new default [`PingConfig`] which can be modified by the user.
    pub fn new() -> Self {
        Self {
            keep_alive: false,
            max_failures: MAX_FAILURES,
            protocol: ProtocolName::from(PROTOCOL_NAME),
            codec: ProtocolCodec::Identity(PING_PAYLOAD_SIZE),
        }
    }

    /// Set maximum failures the protocol.
    pub fn with_max_failure(mut self, max_failures: usize) -> Self {
        self.max_failures = max_failures;
        self
    }

    /// Set keep alive mode for the protocol.
    pub fn with_keep_alive(mut self, keep_alive: bool) -> Self {
        self.keep_alive = keep_alive;
        self
    }

    /// Build [`PingConfig`].
    pub fn build(self) -> (PingConfig, Box<dyn Stream<Item = PingEvent> + Send + Unpin>) {
        let (tx_event, rx_event) = channel(DEFAULT_CHANNEL_SIZE);

        (
            PingConfig {
                tx_event,
                keep_alive: self.keep_alive,
                max_failures: self.max_failures,
                protocol: self.protocol,
                codec: self.codec,
            },
            Box::new(ReceiverStream::new(rx_event)),
        )
    }
}
