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
        websocket::{
            config::TransportConfig, connection::WebSocketConnection, listener::WebSocketListener,
        },
        Transport,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use url::Url;

use std::{collections::HashMap, time::Duration};

pub(crate) use substream::Substream;

mod connection;
mod listener;
mod stream;
mod substream;

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
const LOG_TARGET: &str = "litep2p::websocket";

/// WebSocket transport.
pub(crate) struct WebSocketTransport {
    /// Transport context.
    context: TransportHandle,

    /// Transport configuration.
    config: TransportConfig,

    /// WebSocket listener.
    listener: WebSocketListener,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections:
        FuturesUnordered<BoxFuture<'static, Result<WebSocketConnection, WebSocketError>>>,
}

impl WebSocketTransport {
    /// Convert `Multiaddr` into `url::Url`
    fn multiaddr_into_url(address: Multiaddr) -> crate::Result<Url> {
        let mut protocol_stack = address.iter();

        let dial_address = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Ip4(address) => address.to_string(),
            Protocol::Ip6(address) => format!("[{}]", address.to_string()),
            Protocol::Dns(address) | Protocol::Dns4(address) | Protocol::Dns6(address) =>
                address.to_string(),

            _ => return Err(Error::TransportNotSupported(address)),
        };

        let url = match protocol_stack
            .next()
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
        {
            Protocol::Tcp(port) => match protocol_stack.next() {
                Some(Protocol::Ws(_)) => format!("ws://{dial_address}:{port}/"),
                Some(Protocol::Wss(_)) => format!("wss://{dial_address}:{port}/"),
                _ => return Err(Error::TransportNotSupported(address.clone())),
            },
            _ => return Err(Error::TransportNotSupported(address)),
        };

        tracing::trace!(target: LOG_TARGET, ?url, "parse address");

        url::Url::parse(&url).map_err(|_| Error::InvalidData)
    }
}

#[async_trait::async_trait]
impl Transport for WebSocketTransport {
    type Config = TransportConfig;

    /// Create new [`Transport`] object.
    async fn new(context: TransportHandle, mut config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_addresses = ?config.listen_addresses,
            "start websocket transport",
        );

        Ok(Self {
            listener: WebSocketListener::new(std::mem::replace(
                &mut config.listen_addresses,
                Vec::new(),
            )),
            config,
            context,
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Vec<Multiaddr> {
        self.listener.listen_addresses().cloned().collect()
    }

    /// Poll next connection.
    async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                connection = self.listener.next() => match connection {
                    None => return Err(Error::EssentialTaskClosed),
                    Some(Err(error)) => return Err(error.into()),
                    Some(Ok((stream, address))) => {
                        let connection_id = self.context.next_connection_id();
                        let context = self.context.protocol_set(connection_id);
                        let yamux_config = self.config.yamux_config.clone();
                        let bandwidth_sink = self.context.bandwidth_sink.clone();

                        self.pending_connections.push(Box::pin(async move {
                            match tokio::time::timeout(Duration::from_secs(10), async move {
                                WebSocketConnection::accept_connection(
                                    stream,
                                    address,
                                    connection_id,
                                    yamux_config,
                                    bandwidth_sink,
                                    context,
                                )
                                .await
                                .map_err(|error| WebSocketError::new(error, None))
                            }).await {
                                Err(_) => Err(WebSocketError::new(Error::Timeout, None)),
                                Ok(Err(error)) => Err(error),
                                Ok(Ok(result)) => Ok(result),
                            }
                        }));
                    }
                },
                command = self.context.next() => match command.ok_or(Error::EssentialTaskClosed)? {
                    TransportManagerCommand::Dial { address, connection } => {
                        let context = self.context.protocol_set(connection);
                        let yamux_config = self.config.yamux_config.clone();
                        let peer = match address.iter().find(
                            |protocol| std::matches!(protocol, Protocol::P2p(_))
                        ) {
                            Some(Protocol::P2p(peer)) => PeerId::from_multihash(peer).expect("to succeed"),
                            _ => {
                                tracing::error!(target: LOG_TARGET, ?address, "state mismatch: multiaddress doesn't contain peer id");
                                debug_assert!(false);

                                self.context.report_dial_failure(
                                    connection,
                                    address.clone(),
                                    Error::AddressError(AddressError::PeerIdMissing)
                                ).await;
                                continue;
                            }
                        };

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
                        let bandwidth_sink = self.context.bandwidth_sink.clone();
                        self.pending_dials.insert(connection, address.clone());

                        self.pending_connections.push(Box::pin(async move {
                            match tokio::time::timeout(Duration::from_secs(10), async move {
                                WebSocketConnection::open_connection(
                                    address,
                                    Some(peer),
                                    ws_address,
                                    connection,
                                    yamux_config,
                                    bandwidth_sink,
                                    context,
                                )
                                .await
                                .map_err(|error| {
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

                            self.context.executor.run(Box::pin(async move {
                                if let Err(error) = connection.start().await {
                                    tracing::debug!(target: LOG_TARGET, ?error, "connection failed");
                                }
                            }))
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
