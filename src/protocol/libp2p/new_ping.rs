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
    connection::ConnectionEvent,
    protocol::libp2p::Libp2pProtocolEvent,
    protocol::{Codec, ExecutionContext, Protocol},
    transport::{
        substream::{Substream, SubstreamSet},
        Connection, TransportEvent,
    },
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use futures::{AsyncReadExt, AsyncWriteExt, Stream};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::ping";

/// IPFS Identify protocol name
pub const PROTOCOL_STRING: &str = "/ipfs/ping/1.0.0";

pub const PROTOCOL_NAME: ProtocolName = ProtocolName::from_static_str(PROTOCOL_STRING);

/// Size for `/ipfs/ping/1.0.0` payloads.
const PING_PAYLOAD_SIZE: usize = 32;

#[derive(Debug)]
enum PingEvent {}

struct Ping<S: Substream> {
    event_tx: Sender<PingEvent>,
    connection_rx: Receiver<ConnectionEvent<S>>,
    substreams: SubstreamSet<S>,
}

impl<S: Substream> Ping<S> {
    /// Create new [`Ping`] protocol.
    pub fn new(
        event_tx: Sender<PingEvent>,
        connection_rx: Receiver<ConnectionEvent<S>>,
        substreams: SubstreamSet<S>,
    ) -> Self {
        Self {
            event_tx,
            connection_rx,
            substreams,
        }
    }
}

impl<S: Substream, C: Codec> Protocol<C> for Ping<S> {
    type Event = PingEvent;
    type Context = ();

    /// Initialize protocol and return its event stream for the caller.
    fn new(
        protocol: ProtocolName,
        context: Option<Self::Context>,
    ) -> (Self, Box<dyn Stream<Item = Self::Event> + Send>)
    where
        Self: Sized,
    {
        let (event_tx, event_rx) = channel(64);
        let (connection_tx, connection_rx) = channel(64);
        let ping = Ping::new(event_tx, connection_rx, SubstreamSet::new());

        (ping, Box::new(ReceiverStream::new(event_rx)))
    }

    /// Start protocol executor.
    fn run<E: ExecutionContext>(&mut self, exec_context: E) -> crate::Result<()> {
        todo!();
    }
}
