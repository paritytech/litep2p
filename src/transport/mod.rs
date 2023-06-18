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
    config::TransportConfig,
    crypto::{ed25519::Keypair, noise::NoiseConfiguration},
    error::Error,
    new::TransportContext,
    new_config::Config,
    peer_id::PeerId,
    protocol::ProtocolSet,
    types::{protocol::ProtocolName, ProtocolId, ProtocolType, RequestId, SubstreamId},
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
pub mod tcp_new;

// TODO: only return opaque connection object
// TODO: add muxer only on upper level
// TODO: move substream events away from here

// TODO: protocols for substream events
/// Supported transport types.
pub enum TransportType {
    /// TCP.
    Tcp(Multiaddr),
}

pub trait Connection: AsyncRead + AsyncWrite + Unpin + Send + Debug + 'static {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Debug + 'static> Connection for T {}

#[derive(Debug)]
pub enum Direction {
    /// Substream is inbound
    Inbound,
    /// Substream is outbound
    Outbound,
}

/// Events emitted by the underlying transport.
#[derive(Debug)]
pub enum TransportEvent {
    SubstreamOpened(String, PeerId, Direction, Box<dyn Connection>),
    SubstreamClosed(String, PeerId),
    ConnectionEstablished(PeerId),
    ConnectionClosed(PeerId),
    DialFailure(Multiaddr),
    SubstreamOpenFailure(String, PeerId, Error),
}

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
    // TODO: remove
    async fn open_substream(&mut self, protocol: String, peer: PeerId, handshake: Vec<u8>);
}

// TODO: make this into a poll so backpressure can be signaled easily

#[async_trait::async_trait]
pub trait Transport {
    /// Start the underlying transport listener and return a handle which allows `litep2p` to
    // interact with the transport.
    async fn start(
        keypair: &Keypair,
        config: TransportConfig,
        tx: Sender<TransportEvent>,
    ) -> crate::Result<Box<dyn TransportService>>;
}

#[async_trait::async_trait]
pub trait NewTransportService: Send {
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

// // TODO: introduce error type?
// // TODO: introduce `Substream` trait?
// #[async_trait::async_trait]
// pub trait ConnectionNew: Send + Unpin + 'static {
//     type Substream: Debug + AsyncRead + AsyncWrite + Unpin;

//     /// Get remote peer ID.
//     fn peer_id(&self) -> &PeerId;

//     /// Get connection ID.
//     fn connection_id(&self) -> &usize;

//     /// Remote peer's address.
//     fn remote_address(&self) -> &Multiaddr;

//     /// Start connection event loop.
//     async fn start(self, protocol_info: ProtocolSet) -> crate::Result<()>;
// }

/// Trait which allows `litep2p` to associate dial failures to opened connections.
pub trait TransportError {
    /// Get connection ID.
    fn connection_id(&self) -> Option<usize>;

    /// Convert [`TransportError`] into `Error`
    fn into_error(self) -> Error;
}

/// Events emitted by the underlying transport.
#[derive(Debug)]
pub enum NewTransportEvent {
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

// TODO: introduce error type?
#[async_trait::async_trait]
pub trait TransportNew {
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
    async fn next_event(&mut self) -> Result<NewTransportEvent, Self::Error>;
}
