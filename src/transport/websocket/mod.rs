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

//! WebSocket transport.

use crate::{
    error::{AddressError, Error},
    transport::{
        manager::{TransportHandle, TransportManagerCommand},
        websocket::{config::TransportConfig, connection::WebSocketConnection},
        Transport,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use tokio::net::TcpListener;
use url::Url;

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::Duration,
};

mod connection;
mod stream;

pub mod config;

#[derive(Debug)]
pub(super) struct WebSocketError {
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

    /// Convert `Multiaddr` into `url::Url`
    fn multiaddr_into_url(address: Multiaddr) -> crate::Result<Url> {
        let mut protocol_stack = address.iter();

        let ip_address = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Ip4(address) => address.to_string(),
            Protocol::Ip6(address) => format!("[{}]", address.to_string()),
            _ => return Err(Error::TransportNotSupported(address)),
        };

        match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Tcp(port) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) => {
                    let ws_address = format!("ws://{ip_address}:{port}/");

                    tracing::trace!(target: LOG_TARGET, ?ws_address, "parse address");

                    url::Url::parse(&ws_address).map_err(|_| Error::InvalidData)
                }
                _ => return Err(Error::TransportNotSupported(address.clone())),
            },
            _ => Err(Error::TransportNotSupported(address)),
        }
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
                            match tokio::time::timeout(Duration::from_secs(10), async move {
                                WebSocketConnection::accept_connection(stream, address, connection_id, yamux_config, context)
                                    .await
                                    .map_err(|error| WebSocketError::new(error, None))
                            }).await {
                                Err(_) => Err(WebSocketError::new(Error::Timeout, None)),
                                Ok(Err(error)) => Err(error),
                                Ok(Ok(result)) => Ok(result),
                            }
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

                        // try to convert the multiaddress into a `Url` and if it fails, report dial failure immediately
                        let ws_address = match Self::multiaddr_into_url(address.clone()) {
                            Ok(address) => address,
                            Err(error) => {
                                self.context.report_dial_failure(connection, address, error).await;
                                continue;
                            }
                        };

                        tracing::debug!(target: LOG_TARGET, ?address, ?connection, "open connection");

                        // TODO: make timeout configurable
                        self.pending_dials.insert(connection, address.clone());
                        self.pending_connections.push(Box::pin(async move {
                            match tokio::time::timeout(Duration::from_secs(10), async move {
                                WebSocketConnection::open_connection(address, ws_address, connection, yamux_config, context)
                                    .await
                                    .map_err(|error| {
                                    tracing::warn!("connection failed: {error:?}");
                                    WebSocketError::new(error, Some(connection))
                                })
                            }).await {
                                Err(_) => Err(WebSocketError::new(Error::Timeout, Some(connection))),
                                Ok(Err(error)) => Err(error),
                                Ok(Ok(result)) => Ok(result),
                            }
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
