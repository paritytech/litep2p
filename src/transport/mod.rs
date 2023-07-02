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
    error::Error, peer_id::PeerId, types::ConnectionId, TransportContext, DEFAULT_CHANNEL_SIZE,
};

use multiaddr::Multiaddr;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::fmt::Debug;

pub mod quic;
pub mod tcp;

/// Commands send by `Litep2p` to the transport.
#[derive(Debug)]
pub(crate) enum TransportCommand {
    /// Dial remote peer at `address`.
    Dial {
        /// Address.
        address: Multiaddr,

        /// Connection ID allocated for this connection.
        connection_id: ConnectionId,
    },
}

/// Events emitted by the underlying transport.
#[derive(Debug)]
pub enum TransportEvent {
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Remote address.
        address: Multiaddr,
    },

    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
    },

    DialFailure {
        /// Dialed address.
        address: Multiaddr,

        /// Error.
        error: Error,
    },
}

#[async_trait::async_trait]
pub(crate) trait Transport {
    type Config: Debug;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self>
    where
        Self: Sized;

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr;

    /// Start transport event loop.
    async fn start(mut self) -> crate::Result<()>;
}
