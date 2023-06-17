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
    protocol::ConnectionEvent,
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
pub const PROTOCOL_NAME: &str = "/ipfs/ping/1.0.0";

/// Size for `/ipfs/ping/1.0.0` payloads.
const PING_PAYLOAD_SIZE: usize = 32;

/// Ping configuration.
pub struct Config {}

impl Config {
    /// Create new [`PingConfig`].
    pub fn new() -> Self {
        Self {}
    }
}

#[derive(Debug)]
pub enum PingEvent {}

struct Ping {}

impl Ping {
    fn new() -> Self {
        Self {}
    }

    async fn run(mut self) {
        while let Some(event) = todo!() {
            match event {
                ConnectionEvent::ConnectionEstablished { peer, connection } => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "connection established");
                    // TODO: store peer information
                }
                ConnectionEvent::ConnectionClosed { peer } => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "connection closed");
                    // TODO: remove peer information
                }
                ConnectionEvent::SubstreamOpened { peer, substream } => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "substream opened");
                    // TODO: handle ping
                }
                ConnectionEvent::SubstreamOpenFailure { peer, error } => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?error,
                        "failed to open substream"
                    );
                }
            }
        }
    }
}
