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
    peer_id::PeerId,
    protocol::{Codec, Protocol, ProtocolBuilder, SubstreamService},
    substream::{Substream, SubstreamSet},
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use futures::{AsyncReadExt, AsyncWriteExt, Stream};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;

use std::collections::HashMap;

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::ping";

/// IPFS Ping protocol name as a string.
pub const PROTOCOL_STRING: &str = "/ipfs/ping/1.0.0";

/// IPFS Ping protocol name.
pub const PROTOCOL_NAME: ProtocolName = ProtocolName::from_static_str(PROTOCOL_STRING);

/// Size for `/ipfs/ping/1.0.0` payloads.
const PING_PAYLOAD_SIZE: usize = 32;

struct PingBuilder {
    event_tx: Sender<PingEvent>,
}

impl PingBuilder {
    /// Create new [`PingBuilder`].
    pub fn new() -> (Self, Box<dyn Stream<Item = PingEvent> + Send>) {
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);

        (Self { event_tx }, Box::new(ReceiverStream::new(event_rx)))
    }
}

impl ProtocolBuilder for PingBuilder {
    type Protocol = Ping;

    /// Get protocol name.
    fn protocol_name(&self) -> &ProtocolName {
        return &PROTOCOL_NAME;
    }

    /// Build `Protocol`.
    fn build(self, service: Sender<()>) -> Self::Protocol {
        let PingBuilder { event_tx } = self;

        Ping {
            event_tx,
            service,
            peers: HashMap::new(),
        }
    }
}

#[derive(Debug)]
enum PingEvent {}

struct Ping {
    /// Connected peers.
    peers: HashMap<PeerId, ()>,

    ///
    event_tx: Sender<PingEvent>,

    // TODO: this is used to open substreams
    service: Sender<()>,
}

impl Ping {}

#[async_trait::async_trait]
impl Protocol for Ping {
    type Event = PingEvent;

    /// Start the protocol runner.
    async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting ping event loop");

        todo!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::substream::mock::MockSubstream;
    use futures::Sink;
    use std::{
        pin::Pin,
        task::{Context, Poll},
        time::Duration,
    };
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn initialize_ping() {
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init()
            .expect("to succeed");

        // let (ping, event_stream) = PingBuilder::<MockSubstream>::new();
        // let (tx, rx) = mpsc::channel(64);
        // let (ping, tx_conn) = ping.build(tx);
        // tokio::spawn(ping.run());
        // tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
