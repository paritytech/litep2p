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
    crypto::{ed25519::Keypair, noise::NoiseConfiguration},
    error::Error,
    peer_id::PeerId,
    protocol::ProtocolSet,
    types::protocol::ProtocolName,
    TransportContext,
};

use futures::{
    future::BoxFuture,
    io::{AsyncRead, AsyncWrite},
    Sink, Stream,
};
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{Receiver, Sender};

use std::fmt::Debug;

pub mod tcp;

#[async_trait::async_trait]
pub trait TransportService: Send {
    /// Open connection to remote peer.
    ///
    /// Negotiate `noise`, perform the Noise handshake, negotiate `yamux` and return TODO
    async fn open_connection(&mut self, address: Multiaddr);

    /// Close connection to remote peer.
    async fn close_connection(&mut self, peer: PeerId);

    /// Open substream to remote `peer` for `protocol`.
    ///
    /// The substream is closed by dropping the sink received in [`Litep2p::SubstreamOpened`] message.
    async fn next_connection(&mut self);
}

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
