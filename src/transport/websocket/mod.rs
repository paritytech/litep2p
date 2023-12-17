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
        manager::TransportHandle,
        websocket::{
            config::TransportConfig,
            connection::{NegotiatedConnection, WebSocketConnection},
            listener::WebSocketListener,
        },
        Transport, TransportBuilder, TransportEvent,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use url::Url;

use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

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
        FuturesUnordered<BoxFuture<'static, Result<NegotiatedConnection, WebSocketError>>>,

    /// Negotiated connections waiting validation.
    pending_open: HashMap<ConnectionId, NegotiatedConnection>,
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

impl TransportBuilder for WebSocketTransport {
    type Config = TransportConfig;
    type Transport = WebSocketTransport;

    /// Create new [`Transport`] object.
    fn new(context: TransportHandle, mut config: Self::Config) -> crate::Result<Self>
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
            pending_open: HashMap::new(),
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Vec<Multiaddr> {
        self.listener.listen_addresses().cloned().collect()
    }
}

impl Transport for WebSocketTransport {
    fn dial(&mut self, connection_id: ConnectionId, address: Multiaddr) -> crate::Result<()> {
        let yamux_config = self.config.yamux_config.clone();
        let peer = match address.iter().find(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
            Some(Protocol::P2p(peer)) => PeerId::from_multihash(peer).expect("to succeed"),
            _ => {
                tracing::error!(target: LOG_TARGET, ?address, "state mismatch: multiaddress doesn't contain peer id");
                debug_assert!(false);

                return Err(Error::AddressError(AddressError::PeerIdMissing));
            }
        };

        // try to convert the multiaddress into a `Url` and if it fails,
        // report dial failure immediately
        let keypair = self.context.keypair.clone();
        let ws_address = Self::multiaddr_into_url(address.clone())?;
        self.pending_dials.insert(connection_id, address.clone());

        tracing::debug!(target: LOG_TARGET, ?connection_id, ?address, "open connection");

        self.pending_connections.push(Box::pin(async move {
            match tokio::time::timeout(Duration::from_secs(10), async move {
                WebSocketConnection::open_connection(
                    connection_id,
                    keypair,
                    address,
                    Some(peer),
                    ws_address,
                    yamux_config,
                )
                .await
                .map_err(|error| WebSocketError::new(error, Some(connection_id)))
            })
            .await
            {
                Err(_) => Err(WebSocketError::new(Error::Timeout, Some(connection_id))),
                Ok(Err(error)) => Err(error),
                Ok(Ok(result)) => Ok(result),
            }
        }));

        Ok(())
    }

    fn accept(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        let context = self
            .pending_open
            .remove(&connection_id)
            .ok_or(Error::ConnectionDoesntExist(connection_id))?;
        let protocol_set = self.context.protocol_set(connection_id);
        let bandwidth_sink = self.context.bandwidth_sink.clone();

        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "start connection",
        );

        self.context.executor.run(Box::pin(async move {
            if let Err(error) =
                WebSocketConnection::new(context, protocol_set, bandwidth_sink).start().await
            {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?connection_id,
                    ?error,
                    "connection exited with error",
                );
            }
        }));

        Ok(())
    }

    fn reject(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        self.pending_open
            .remove(&connection_id)
            .map_or(Err(Error::ConnectionDoesntExist(connection_id)), |_| Ok(()))
    }
}

impl Stream for WebSocketTransport {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(connection)) = self.listener.poll_next_unpin(cx) {
            match connection {
                Err(_) => return Poll::Ready(None),
                Ok((stream, address)) => {
                    let connection_id = self.context.next_connection_id();
                    let keypair = self.context.keypair.clone();
                    let yamux_config = self.config.yamux_config.clone();

                    self.pending_connections.push(Box::pin(async move {
                        match tokio::time::timeout(Duration::from_secs(10), async move {
                            WebSocketConnection::accept_connection(
                                stream,
                                connection_id,
                                keypair,
                                address,
                                yamux_config,
                            )
                            .await
                            .map_err(|error| WebSocketError::new(error, None))
                        })
                        .await
                        {
                            Err(_) => Err(WebSocketError::new(Error::Timeout, None)),
                            Ok(Err(error)) => Err(error),
                            Ok(Ok(result)) => Ok(result),
                        }
                    }));
                }
            }
        }

        while let Poll::Ready(Some(connection)) = self.pending_connections.poll_next_unpin(cx) {
            match connection {
                Ok(connection) => {
                    let peer = connection.peer();
                    let endpoint = connection.endpoint();
                    self.pending_open.insert(connection.connection_id(), connection);

                    return Poll::Ready(Some(TransportEvent::ConnectionEstablished {
                        peer,
                        endpoint,
                    }));
                }
                Err(error) => match error.connection_id {
                    Some(connection_id) => match self.pending_dials.remove(&connection_id) {
                        Some(address) =>
                            return Poll::Ready(Some(TransportEvent::DialFailure {
                                connection_id,
                                address,
                                error: error.error,
                            })),
                        None => {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection")
                        }
                    },
                    None => {
                        tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection")
                    }
                },
            }
        }

        Poll::Pending
    }
}
