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
    error::{AddressError, Error},
    peer_id::PeerId,
    transport::{Transport, TransportError, TransportEvent},
    TransportContext,
};

use multiaddr::{Multiaddr, Protocol};
use s2n_quic::Server;

use std::net::{IpAddr, SocketAddr};

/// Logging target for the file.
const LOG_TARGET: &str = "tcp";

#[derive(Debug, Clone)]
pub(crate) struct QuicError {}

/// Trait which allows `litep2p` to associate dial failures to opened connections.
impl TransportError for QuicError {
    /// Get connection ID.
    fn connection_id(&self) -> Option<usize> {
        None
    }

    /// Convert [`TransportError`] into `Error`
    fn into_error(self) -> Error {
        todo!();
    }
}

/// QUIC configuration.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    /// QUIC listen address.
    listen_address: Multiaddr,

    /// Local peer ID.
    peer: PeerId,
}

/// QUIC transport object.
#[derive(Debug)]
pub(crate) struct QuicTransport {
    /// QUIC server.
    server: Server,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Next connection ID.
    next_connection_id: usize,
}

impl QuicTransport {
    /// Extract socket address and `PeerId`, if found, from `address`.
    fn get_socket_address(address: &Multiaddr) -> crate::Result<(SocketAddr, Option<PeerId>)> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse multi address");

        let mut iter = address.iter();
        let socket_address = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Tcp(port)) => SocketAddr::new(IpAddr::V6(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Tcp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            Some(Protocol::Ip4(address)) => match iter.next() {
                Some(Protocol::Tcp(port)) => SocketAddr::new(IpAddr::V4(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Tcp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            protocol => {
                tracing::error!(target: LOG_TARGET, ?protocol, "invalid transport protocol");
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        let maybe_peer = match iter.next() {
            Some(Protocol::P2p(multihash)) => Some(PeerId::from_multihash(multihash)?),
            None => None,
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `P2p` or `None`"
                );
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        Ok((socket_address, maybe_peer))
    }
}

#[async_trait::async_trait]
impl Transport for QuicTransport {
    type Config = Config;
    type Error = QuicError;

    /// Create new [`QuicTransport`] object.
    async fn new(_context: TransportContext, config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized,
    {
        todo!();
        // tracing::info!(
        //     target: LOG_TARGET,
        //     listen_address = ?config.listen_address,
        //     "start quic transport",
        // );

        // let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;

        // let mut server = Server::builder()
        //     .with_tls((CERT_PEM, KEY_PEM))?
        //     .with_io(listen_address)?
        //     .start()?;
        // let listen_address = server.local_addr()?;

        // Ok(Self {
        //     server,
        //     listen_address,
        //     next_connection_id: 0usize,
        // })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        todo!();
    }

    /// Try to open a connection to remote peer.
    ///
    /// The result is polled using [`Transport::next_connection()`].
    fn open_connection(&mut self, _address: Multiaddr) -> crate::Result<usize> {
        todo!();
    }

    /// Poll next connection.
    async fn next_event(&mut self) -> Result<TransportEvent, Self::Error> {
        todo!();
    }
}
