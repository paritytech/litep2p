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

//! TCP transport.

use crate::{
    config::Role,
    error::Error,
    transport::{
        manager::TransportHandle,
        tcp::{
            config::TransportConfig,
            connection::{NegotiatedConnection, TcpConnection},
            listener::{AddressType, TcpListener},
        },
        Transport, TransportBuilder, TransportEvent,
    },
    types::ConnectionId,
};

use futures::{
    future::BoxFuture,
    stream::{FuturesUnordered, Stream, StreamExt},
};
use multiaddr::Multiaddr;
use tokio::net::TcpStream;

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};

pub(crate) use substream::Substream;

mod connection;
mod listener;
mod substream;

pub mod config;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::tcp";

/// TCP transport.
pub(crate) struct TcpTransport {
    /// Transport context.
    context: TransportHandle,

    /// Transport configuration.
    config: TransportConfig,

    /// TCP listener.
    listener: TcpListener,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending opening connections.
    pending_connections:
        FuturesUnordered<BoxFuture<'static, Result<NegotiatedConnection, (ConnectionId, Error)>>>,

    /// Pending raw, unnegotiated connections.
    pending_raw_connections: FuturesUnordered<
        BoxFuture<'static, Result<(ConnectionId, Multiaddr, TcpStream), ConnectionId>>,
    >,

    /// Opened raw connection, waiting for approval/rejection from `TransportManager`.
    opened_raw: HashMap<ConnectionId, (TcpStream, Multiaddr)>,

    /// Canceled raw connections.
    canceled: HashSet<ConnectionId>,

    /// Connections which have been opened and negotiated but are being validated by the
    /// `TransportManager`.
    pending_open: HashMap<ConnectionId, NegotiatedConnection>,
}

impl TcpTransport {
    /// Handle inbound TCP connection.
    fn on_inbound_connection(&mut self, connection: TcpStream, address: SocketAddr) {
        let connection_id = self.context.next_connection_id();
        let yamux_config = self.config.yamux_config.clone();
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let connection_open_timeout = self.config.connection_open_timeout;
        let substream_open_timeout = self.config.substream_open_timeout;
        let keypair = self.context.keypair.clone();

        self.pending_connections.push(Box::pin(async move {
            TcpConnection::accept_connection(
                connection,
                connection_id,
                keypair,
                address,
                yamux_config,
                max_read_ahead_factor,
                max_write_buffer_size,
                connection_open_timeout,
                substream_open_timeout,
            )
            .await
            .map_err(|error| (connection_id, error))
        }));
    }
}

impl TransportBuilder for TcpTransport {
    type Config = TransportConfig;
    type Transport = TcpTransport;

    /// Create new [`TcpTransport`].
    fn new(
        context: TransportHandle,
        mut config: Self::Config,
    ) -> crate::Result<(Self, Vec<Multiaddr>)> {
        tracing::info!(
            target: LOG_TARGET,
            listen_addresses = ?config.listen_addresses,
            "start tcp transport",
        );

        // start tcp listeners for all listen addresses
        let (listener, listen_addresses) =
            TcpListener::new(std::mem::replace(&mut config.listen_addresses, Vec::new()));

        Ok((
            Self {
                listener,
                config,
                context,
                canceled: HashSet::new(),
                opened_raw: HashMap::new(),
                pending_open: HashMap::new(),
                pending_dials: HashMap::new(),
                pending_connections: FuturesUnordered::new(),
                pending_raw_connections: FuturesUnordered::new(),
            },
            listen_addresses,
        ))
    }
}

impl Transport for TcpTransport {
    fn dial(&mut self, connection_id: ConnectionId, address: Multiaddr) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?connection_id, ?address, "open connection");

        let (socket_address, peer) = listener::TcpListener::get_socket_address(&address)?;
        let yamux_config = self.config.yamux_config.clone();
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let connection_open_timeout = self.config.connection_open_timeout;
        let substream_open_timeout = self.config.substream_open_timeout;
        let keypair = self.context.keypair.clone();

        self.pending_dials.insert(connection_id, address.clone());
        self.pending_connections.push(Box::pin(async move {
            TcpConnection::open_connection(
                connection_id,
                keypair,
                socket_address,
                peer,
                yamux_config,
                max_read_ahead_factor,
                max_write_buffer_size,
                connection_open_timeout,
                substream_open_timeout,
            )
            .await
            .map_err(|error| (connection_id, error))
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
        let next_substream_id = self.context.next_substream_id.clone();

        tracing::trace!(
            target: LOG_TARGET,
            ?connection_id,
            "start connection",
        );

        self.context.executor.run(Box::pin(async move {
            if let Err(error) =
                TcpConnection::new(context, protocol_set, bandwidth_sink, next_substream_id)
                    .start()
                    .await
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
        self.canceled.insert(connection_id);
        self.pending_open
            .remove(&connection_id)
            .map_or(Err(Error::ConnectionDoesntExist(connection_id)), |_| Ok(()))
    }

    fn open(
        &mut self,
        connection_id: ConnectionId,
        addresses: Vec<Multiaddr>,
    ) -> crate::Result<()> {
        let connection_open_timeout = self.config.connection_open_timeout;
        let mut futures: FuturesUnordered<_> = addresses
            .into_iter()
            .map(|address| async move {
                let (socket_address, _) = TcpListener::get_socket_address(&address)?;

                match tokio::time::timeout(connection_open_timeout, async move {
                    match &socket_address {
                        AddressType::Socket(socket_address) =>
                            TcpStream::connect(socket_address).await,
                        AddressType::Dns(address, port) =>
                            TcpStream::connect(format!("{address}:{port}")).await,
                    }
                })
                .await
                {
                    Err(_) => Err(Error::Timeout),
                    Ok(Err(error)) => Err(error.into()),
                    Ok(Ok(stream)) => Ok((address, stream)),
                }
            })
            .collect();

        self.pending_raw_connections.push(Box::pin(async move {
            while let Some(result) = futures.next().await {
                match result {
                    Ok((address, stream)) => return Ok((connection_id, address, stream)),
                    Err(error) => tracing::debug!(
                        target: LOG_TARGET,
                        ?connection_id,
                        ?error,
                        "failed to open connection",
                    ),
                }
            }

            Err(connection_id)
        }));

        Ok(())
    }

    fn negotiate(&mut self, connection_id: ConnectionId) -> crate::Result<()> {
        let (stream, address) = self
            .opened_raw
            .remove(&connection_id)
            .ok_or(Error::ConnectionDoesntExist(connection_id))?;

        let (socket_address, peer) = listener::TcpListener::get_socket_address(&address)?;
        let yamux_config = self.config.yamux_config.clone();
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let connection_open_timeout = self.config.connection_open_timeout;
        let substream_open_timeout = self.config.substream_open_timeout;
        let keypair = self.context.keypair.clone();

        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?connection_id,
            ?address,
            "negotiate connection",
        );

        self.pending_dials.insert(connection_id, address);
        self.pending_connections.push(Box::pin(async move {
            match tokio::time::timeout(connection_open_timeout, async move {
                TcpConnection::negotiate_connection(
                    stream,
                    peer,
                    connection_id,
                    keypair,
                    Role::Dialer,
                    socket_address,
                    yamux_config,
                    max_read_ahead_factor,
                    max_write_buffer_size,
                    substream_open_timeout,
                )
                .await
                .map_err(|error| (connection_id, error))
            })
            .await
            {
                Err(_) => Err((connection_id, Error::Timeout)),
                Ok(Err(error)) => Err(error),
                Ok(Ok(connection)) => Ok(connection),
            }
        }));

        Ok(())
    }

    fn cancel(&mut self, connection_id: ConnectionId) {
        self.canceled.insert(connection_id);
    }
}

impl Stream for TcpTransport {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(event) = self.listener.poll_next_unpin(cx) {
            match event {
                None | Some(Err(_)) => return Poll::Ready(None),
                Some(Ok((connection, address))) => {
                    self.on_inbound_connection(connection, address);
                }
            }
        }

        while let Poll::Ready(Some(result)) = self.pending_raw_connections.poll_next_unpin(cx) {
            match result {
                Ok((connection_id, address, stream)) => {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?connection_id,
                        ?address,
                        canceled = self.canceled.contains(&connection_id),
                        "connection opened",
                    );

                    if !self.canceled.remove(&connection_id) {
                        self.opened_raw.insert(connection_id, (stream, address.clone()));

                        return Poll::Ready(Some(TransportEvent::ConnectionOpened {
                            connection_id,
                            address,
                        }));
                    }
                }
                Err(connection_id) =>
                    if !self.canceled.remove(&connection_id) {
                        return Poll::Ready(Some(TransportEvent::OpenFailure { connection_id }));
                    },
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
                Err((connection_id, error)) => {
                    if let Some(address) = self.pending_dials.remove(&connection_id) {
                        return Poll::Ready(Some(TransportEvent::DialFailure {
                            connection_id,
                            address,
                            error,
                        }));
                    }
                }
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::ProtocolCodec,
        crypto::{ed25519::Keypair, PublicKey},
        executor::DefaultExecutor,
        transport::manager::{ProtocolContext, SupportedTransport, TransportManager},
        types::protocol::ProtocolName,
        BandwidthSink, PeerId,
    };
    use multiaddr::Protocol;
    use multihash::Multihash;
    use std::{collections::HashSet, sync::Arc};
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn connect_and_accept_works() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, _rx1) = channel(64);
        let (event_tx1, _event_rx1) = channel(64);
        let bandwidth_sink = BandwidthSink::new();

        let handle1 = crate::transport::manager::TransportHandle {
            executor: Arc::new(DefaultExecutor {}),
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair1.clone(),
            tx: event_tx1,
            bandwidth_sink: bandwidth_sink.clone(),

            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx1,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: Vec::new(),
                },
            )]),
        };
        let transport_config1 = TransportConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        };

        let (mut transport1, listen_addresses) =
            TcpTransport::new(handle1, transport_config1).unwrap();
        let listen_address = listen_addresses[0].clone();

        let keypair2 = Keypair::generate();
        let (tx2, _rx2) = channel(64);
        let (event_tx2, _event_rx2) = channel(64);

        let handle2 = crate::transport::manager::TransportHandle {
            executor: Arc::new(DefaultExecutor {}),
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair2.clone(),
            tx: event_tx2,
            bandwidth_sink: bandwidth_sink.clone(),

            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx2,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: Vec::new(),
                },
            )]),
        };
        let transport_config2 = TransportConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        };

        let (mut transport2, _) = TcpTransport::new(handle2, transport_config2).unwrap();
        transport2.dial(ConnectionId::new(), listen_address).unwrap();

        let (res1, res2) = tokio::join!(transport1.next(), transport2.next());

        assert!(std::matches!(
            res1,
            Some(TransportEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Some(TransportEvent::ConnectionEstablished { .. })
        ));
    }

    #[tokio::test]
    async fn dial_failure() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, _rx1) = channel(64);
        let (event_tx1, mut event_rx1) = channel(64);
        let bandwidth_sink = BandwidthSink::new();

        let handle1 = crate::transport::manager::TransportHandle {
            executor: Arc::new(DefaultExecutor {}),
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair1.clone(),
            tx: event_tx1,
            bandwidth_sink: bandwidth_sink.clone(),

            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx1,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: Vec::new(),
                },
            )]),
        };
        let (mut transport1, _) = TcpTransport::new(handle1, Default::default()).unwrap();

        tokio::spawn(async move {
            while let Some(event) = transport1.next().await {
                match event {
                    TransportEvent::ConnectionEstablished { .. } => {}
                    TransportEvent::ConnectionClosed { .. } => {}
                    TransportEvent::DialFailure { .. } => {}
                    TransportEvent::ConnectionOpened { .. } => {}
                    TransportEvent::OpenFailure { .. } => {}
                }
            }
        });

        let keypair2 = Keypair::generate();
        let (tx2, _rx2) = channel(64);
        let (event_tx2, _event_rx2) = channel(64);

        let handle2 = crate::transport::manager::TransportHandle {
            executor: Arc::new(DefaultExecutor {}),
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair2.clone(),
            tx: event_tx2,
            bandwidth_sink: bandwidth_sink.clone(),

            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx2,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: Vec::new(),
                },
            )]),
        };

        let (mut transport2, _) = TcpTransport::new(handle2, Default::default()).unwrap();

        let peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public()));
        let peer2: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public()));

        tracing::info!(target: LOG_TARGET, "peer1 {peer1}, peer2 {peer2}");

        let address = Multiaddr::empty()
            .with(Protocol::Ip6(std::net::Ipv6Addr::new(
                0, 0, 0, 0, 0, 0, 0, 1,
            )))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer1.to_bytes()).unwrap(),
            ));

        transport2.dial(ConnectionId::new(), address).unwrap();

        // spawn the other conection in the background as it won't return anything
        tokio::spawn(async move {
            loop {
                let _ = event_rx1.recv().await;
            }
        });

        assert!(std::matches!(
            transport2.next().await,
            Some(TransportEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn dial_error_reported_for_outbound_connections() {
        let (mut manager, _handle) = TransportManager::new(
            Keypair::generate(),
            HashSet::new(),
            BandwidthSink::new(),
            8usize,
        );
        let handle = manager.transport_handle(Arc::new(DefaultExecutor {}));
        manager.register_transport(
            SupportedTransport::Tcp,
            Box::new(crate::transport::dummy::DummyTransport::new()),
        );
        let (mut transport, _) = TcpTransport::new(
            handle,
            TransportConfig {
                listen_addresses: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
                ..Default::default()
            },
        )
        .unwrap();

        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::Ip4(std::net::Ipv4Addr::new(255, 254, 253, 252)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        manager.dial_address(multiaddr.clone()).await.unwrap();

        assert!(transport.pending_dials.is_empty());

        match transport.dial(ConnectionId::from(0usize), multiaddr) {
            Ok(()) => {}
            _ => panic!("invalid result for `on_dial_peer()`"),
        }

        assert!(!transport.pending_dials.is_empty());
        transport.pending_connections.push(Box::pin(async move {
            Err((ConnectionId::from(0usize), Error::Unknown))
        }));

        assert!(std::matches!(
            transport.next().await,
            Some(TransportEvent::DialFailure { .. })
        ));
        assert!(transport.pending_dials.is_empty());
    }
}
