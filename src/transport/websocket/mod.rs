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
    transport::{
        websocket::{config::Config, connection::WebSocketConnection},
        Transport, TransportCommand, TransportContext,
    },
    types::ConnectionId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use tokio::{net::TcpListener, sync::mpsc::Receiver};

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
};

mod connection;

pub mod config;

/// Logging target for the file.
const LOG_TARGET: &str = "websocket";

/// WebSocket transport.
pub(crate) struct WebSocketTransport {
    /// Transport context.
    context: TransportContext,

    /// TCP listener.
    listener: TcpListener,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Next connection ID.
    next_connection_id: ConnectionId,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections: FuturesUnordered<BoxFuture<'static, Result<(), ()>>>,

    /// RX channel for receiving commands from `Litep2p`.
    rx: Receiver<TransportCommand>,
}

impl WebSocketTransport {
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

        // verify that `/ws`/`/wss` is part of the multi address
        match iter.next() {
            Some(Protocol::Ws(_address)) => {}
            Some(Protocol::Wss(_address)) => unimplemented!("secure websocket not implemented"),
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `Ws` or `Wss`"
                );
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
impl Transport for WebSocketTransport {
    type Config = Config;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start websocket transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let listener = TcpListener::bind(listen_address).await?;
        let listen_address = listener.local_addr()?;

        Ok(Self {
            rx,
            context,
            listener,
            listen_address,
            next_connection_id: ConnectionId::new(),
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        // TODO: zzz
        "/ip4/127.0.0.1/tcp/8888/ws".parse().unwrap()
    }

    /// Poll next connection.
    async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                connection = self.listener.accept() => match connection {
                    Ok((stream, address)) => {
                        let context = self.context.clone();

                        // self.pendig_connections.push(Box::pin(async move {
                        let connection = WebSocketConnection::accept_connection(stream, address, context).await;
                        // }));

                    }
                    Err(error) => {
                        tracing::error!(target: LOG_TARGET, ?error, "failed to accept connection");
                    }
                },
                command = self.rx.recv() => match command.ok_or(Error::EssentialTaskClosed)? {
                    TransportCommand::Dial { address, connection_id } => {
                        todo!();
                    }
                }
            }
        }
    }
}
