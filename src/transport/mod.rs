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

use crate::{error::Error, peer_id::PeerId, TransportContext};

use multiaddr::Multiaddr;

use std::fmt::Debug;

pub mod quic;
pub mod tcp;

/// Trait which allows `litep2p` to associate dial failures to opened connections.
pub trait TransportError {
    /// Get connection ID.
    fn connection_id(&self) -> Option<usize>;

    /// Convert [`TransportError`] into `Error`
    fn into_error(self) -> Error;
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
pub trait Transport {
    type Config: Debug;
    type Error: TransportError; // TODO: is this really necessary?

    /// Create new [`Transport`] object.
    async fn new(context: TransportContext, config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized;

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr;

    /// Try to open a connection to remote peer.
    ///
    /// The result is polled using [`Transport::next_connection()`].
    fn open_connection(&mut self, address: Multiaddr) -> crate::Result<usize>;

    /// Poll next connection.
    async fn next_event(&mut self) -> Result<TransportEvent, Self::Error>;
}
