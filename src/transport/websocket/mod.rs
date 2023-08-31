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
        manager::{TransportHandle, TransportManagerCommand},
        websocket::{config::TransportConfig, connection::WebSocketConnection},
        Transport,
    },
    types::ConnectionId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use tokio::net::TcpListener;

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

mod connection;
mod stream;

pub mod config;

#[derive(Debug)]
pub struct WebSocketError {
    /// Error.
    error: Error,

    /// Connection ID.
    connection_id: Option<ConnectionId>,
}

impl WebSocketError {
    pub fn new(error: Error, connection_id: Option<ConnectionId>) -> Self {
        Self {
            error,
            connection_id,
        }
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "websocket";

/// WebSocket transport.
pub(crate) struct WebSocketTransport {
    /// Transport context.
    context: TransportHandle,

    /// Transport configuration.
    config: TransportConfig,

    /// TCP listener.
    listener: TcpListener,

    /// Assigned listen addresss.
    listen_address: Multiaddr,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections:
        FuturesUnordered<BoxFuture<'static, Result<WebSocketConnection, WebSocketError>>>,
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
    type Config = TransportConfig;

    /// Create new [`Transport`] object.
    async fn new(context: TransportHandle, config: Self::Config) -> crate::Result<Self>
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
        let listen_address = Multiaddr::empty()
            .with(Protocol::from(listen_address.ip()))
            .with(Protocol::Tcp(listen_address.port()))
            .with(Protocol::Ws(std::borrow::Cow::Owned("/".to_string())));

        Ok(Self {
            config,
            context,
            listener,
            listen_address,
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        self.listen_address.clone()
    }

    /// Poll next connection.
    async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                connection = self.listener.accept() => match connection {
                    Ok((stream, address)) => {
                        let context = self.context.protocol_set();
                        let connection_id = self.context.next_connection_id();
                        let yamux_config = self.config.yamux_config.clone();

                        self.pending_connections.push(Box::pin(async move {
                            WebSocketConnection::accept_connection(stream, address, connection_id, yamux_config, context)
                                .await
                                .map_err(|error| WebSocketError::new(error, None))
                        }));
                    }
                    Err(error) => {
                        tracing::error!(target: LOG_TARGET, ?error, "failed to accept connection");
                    }
                },
                command = self.context.next() => match command.ok_or(Error::EssentialTaskClosed)? {
                    TransportManagerCommand::Dial { address, connection } => {
                        let context = self.context.protocol_set();
                        let yamux_config = self.config.yamux_config.clone();

                        tracing::debug!(target: LOG_TARGET, ?address, ?connection, "open connection");

                        self.pending_dials.insert(connection, address.clone());
                        self.pending_connections.push(Box::pin(async move {
                            WebSocketConnection::open_connection(address, connection, yamux_config, context)
                                .await
                                .map_err(|error| WebSocketError::new(error, Some(connection)))
                        }));
                    }
                },
                event = self.pending_connections.select_next_some(), if !self.pending_connections.is_empty() => {
                    match event {
                        Ok(connection) => {
                            let _peer = *connection.peer();
                            let _address = connection.address().clone();

                            tokio::spawn(async move {
                                if let Err(error) = connection.start().await {
                                    tracing::debug!(target: LOG_TARGET, ?error, "connection failed");
                                }
                            });
                        }
                        Err(error) => match error.connection_id {
                            Some(connection_id) => match self.pending_dials.remove(&connection_id) {
                                Some(address) => self.context.report_dial_failure(connection_id, address, error.error).await,
                                None => tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection"),
                            }
                            None => tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection"),
                        }
                    }
                }
            }
        }
    }
}
