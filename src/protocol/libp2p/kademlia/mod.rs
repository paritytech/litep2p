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

use crate::{codec::ProtocolCodec, transport::TransportService, types::protocol::ProtocolName};

/// Logging target for the file.
const LOG_TARGET: &str = "kademlia";

/// Protocol name.
const PROTOCOL_NAME: &str = "/ipfs/kad/1.0.0";

/// Kademlia configuration.
#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    pub(crate) protocol: ProtocolName,

    /// Protocol codec.
    pub(crate) codec: ProtocolCodec,
}

impl Config {
    pub fn new() -> (Self, KademliaHandle) {
        (Self {
            protocol: ProtocolName::from(PROTOCOL_NAME),
            codec: ProtocolCodec::UnsignedVarint,
        }, KademliaHandle {})
    }
}

pub struct KademliaHandle {
}

impl KademliaHandle {
    pub fn new() -> Self {
        Self {}
    }
}

pub struct Kademlia {}

impl Kademlia {
    pub fn new(context: TransportService, config: Config) -> Self {
        Self {}
    }

    pub async fn run(self) -> crate::Result<()> {
        tracing::error!(target: LOG_TARGET, "starting kademlia event loop");

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await
        }
    }
}
