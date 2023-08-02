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
    protocol::libp2p::kademlia::handle::{KademliaCommand, KademliaEvent, KademliaHandle},
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};

/// Protocol name.
const PROTOCOL_NAME: &str = "/ipfs/kad/1.0.0";

/// Kademlia replication factor.
const REPLICATION_FACTOR: usize = 20usize;

#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    pub(crate) protocol: ProtocolName,

    /// Protocol codec.
    pub(crate) codec: ProtocolCodec,

    /// Replication factor.
    pub(super) replication_factor: usize,

    /// TX channel for sending events to `KademliaHandle`.
    pub(super) event_tx: Sender<KademliaEvent>,

    /// RX channel for receiving commands from `KademliaHandle`.
    pub(super) cmd_rx: Receiver<KademliaCommand>,
}

/// Kademlia configuration builder.
#[derive(Debug)]
pub struct ConfigBuilder {
    /// Protocol name.
    pub(crate) protocol: ProtocolName,

    /// Protocol codec.
    pub(crate) codec: ProtocolCodec,

    /// Replication factor.
    pub(super) replication_factor: usize,
}

impl ConfigBuilder {
    /// Create new [`Config`].
    pub fn new() -> Self {
        Self {
            protocol: ProtocolName::from(PROTOCOL_NAME),
            codec: ProtocolCodec::UnsignedVarint,
            replication_factor: REPLICATION_FACTOR,
        }
    }

    /// Configuration replication factor.
    pub fn with_replication_factor(mut self, replication_factor: usize) -> Self {
        self.replication_factor = replication_factor;
        self
    }

    /// Build Kademlia configuration.
    pub fn build(self) -> (Config, KademliaHandle) {
        let (cmd_tx, cmd_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Config {
                protocol: self.protocol,
                codec: self.codec,
                replication_factor: self.replication_factor,
                cmd_rx,
                event_tx,
            },
            KademliaHandle::new(cmd_tx, event_rx),
        )
    }
}
