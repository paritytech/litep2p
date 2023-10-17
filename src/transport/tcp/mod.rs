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
    error::{AddressError, Error},
    transport::{
        manager::{TransportHandle, TransportManagerCommand},
        tcp::{config::TransportConfig, connection::TcpConnection},
        Transport,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use multiaddr::{Multiaddr, Protocol};
use tokio::net::{TcpListener, TcpStream};

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

pub(crate) use substream::Substream;

mod connection;
mod substream;

pub mod config;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::tcp";

#[derive(Debug)]
pub(super) struct TcpError {
    /// Error.
    error: Error,

    /// Connection ID.
    connection_id: Option<ConnectionId>,
}

impl TcpError {
    pub fn new(error: Error, connection_id: Option<ConnectionId>) -> Self {
        Self {
            error,
            connection_id,
        }
    }
}

/// TCP transport.
#[derive(Debug)]
pub(crate) struct TcpTransport {
    /// Transport context.
    context: TransportHandle,

    /// Transport configuration.
    config: TransportConfig,

    /// TCP listener.
    listener: TcpListener,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections: FuturesUnordered<BoxFuture<'static, Result<TcpConnection, TcpError>>>,
}

/// Address type.
#[derive(Debug)]
enum AddressType {
    /// Socket address.
    Socket(SocketAddr),

    /// DNS address.
    Dns(String, u16),
}

impl TcpTransport {
    /// Extract socket address and `PeerId`, if found, from `address`.
    fn get_socket_address(address: &Multiaddr) -> crate::Result<(AddressType, Option<PeerId>)> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse multi address");

        let mut iter = address.iter();
        let socket_address = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Tcp(port)) =>
                    AddressType::Socket(SocketAddr::new(IpAddr::V6(address), port)),
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
                Some(Protocol::Tcp(port)) =>
                    AddressType::Socket(SocketAddr::new(IpAddr::V4(address), port)),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Tcp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            Some(Protocol::Dns(address))
            | Some(Protocol::Dns4(address))
            | Some(Protocol::Dns6(address)) => match iter.next() {
                Some(Protocol::Tcp(port)) => AddressType::Dns(address.to_string(), port),
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

    /// Handle inbound TCP connection.
    fn on_inbound_connection(&mut self, connection: TcpStream, address: SocketAddr) {
        let connection_id = self.context.next_connection_id();
        let protocol_set = self.context.protocol_set(connection_id);
        let yamux_config = self.config.yamux_config.clone();
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let bandwidth_sink = self.context.bandwidth_sink.clone();

        self.pending_connections.push(Box::pin(async move {
            TcpConnection::accept_connection(
                protocol_set,
                connection,
                connection_id,
                address,
                yamux_config,
                max_read_ahead_factor,
                max_write_buffer_size,
                bandwidth_sink,
            )
            .await
            .map_err(|error| TcpError::new(error, Some(connection_id)))
        }));
    }

    /// Handle established TCP connection.
    async fn on_connection_established(&mut self, connection: Result<TcpConnection, TcpError>) {
        tracing::trace!(target: LOG_TARGET, failed = ?connection.is_err(), "handle finished tcp connection");

        match connection {
            Ok(connection) => {
                let _peer = *connection.peer();
                let _address = connection.address().clone();

                tokio::spawn(async move {
                    if let Err(error) = connection.start().await {
                        tracing::error!(target: LOG_TARGET, ?error, "connection failure");
                    }
                });
            }
            Err(error) => match error.connection_id {
                Some(connection_id) => match self.pending_dials.remove(&connection_id) {
                    Some(address) =>
                        self.context.report_dial_failure(connection_id, address, error.error).await,
                    None => tracing::debug!(
                        target: LOG_TARGET,
                        ?error,
                        "failed to establish connection"
                    ),
                },
                None => {
                    tracing::debug!(target: LOG_TARGET, ?error, "failed to establish connection")
                }
            },
        }
    }

    /// Dial remote peer.
    async fn on_dial_peer(
        &mut self,
        address: Multiaddr,
        connection_id: ConnectionId,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?address, ?connection_id, "open connection");

        let protocol_set = self.context.protocol_set(connection_id);
        let (socket_address, peer) = Self::get_socket_address(&address)?;
        let yamux_config = self.config.yamux_config.clone();
        let max_read_ahead_factor = self.config.noise_read_ahead_frame_count;
        let max_write_buffer_size = self.config.noise_write_buffer_size;
        let bandwidth_sink = self.context.bandwidth_sink.clone();

        self.pending_dials.insert(connection_id, address);
        self.pending_connections.push(Box::pin(async move {
            TcpConnection::open_connection(
                protocol_set,
                connection_id,
                socket_address,
                peer,
                yamux_config,
                max_read_ahead_factor,
                max_write_buffer_size,
                bandwidth_sink,
            )
            .await
            .map_err(|error| TcpError::new(error, Some(connection_id)))
        }));

        Ok(())
    }
}

#[async_trait::async_trait]
impl Transport for TcpTransport {
    type Config = TransportConfig;

    /// Create new [`TcpTransport`].
    async fn new(
        context: crate::transport::manager::TransportHandle,
        config: Self::Config,
    ) -> crate::Result<Self> {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start tcp transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let listener = match listen_address {
            AddressType::Socket(socket_address) => TcpListener::bind(socket_address).await?,
            AddressType::Dns(_, _) =>
                return Err(Error::TransportNotSupported(config.listen_address)),
        };
        let listen_address = listener.local_addr()?;

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
        Multiaddr::empty()
            .with(Protocol::from(self.listen_address.ip()))
            .with(Protocol::Tcp(self.listen_address.port()))
    }

    /// Start TCP transport event loop.
    async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                connection = self.listener.accept() => match connection {
                    Ok((connection, address)) => self.on_inbound_connection(connection, address),
                    Err(error) => {
                        tracing::debug!(target: LOG_TARGET, ?error, "tcp listener shut down");
                        return Ok(())
                    }
                },
                connection = self.pending_connections.select_next_some(), if !self.pending_connections.is_empty() => {
                    self.on_connection_established(connection).await;
                }
                command = self.context.next() => match command {
                    None => return Err(Error::EssentialTaskClosed),
                    Some(command) => match command {
                        TransportManagerCommand::Dial { address, connection } => {
                            if let Err(error) = self.on_dial_peer(address.clone(), connection).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?address,
                                    ?connection,
                                    ?error,
                                    "failed to dial peer"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::{
        codec::ProtocolCodec,
        crypto::{ed25519::Keypair, PublicKey},
        transport::manager::{
            ProtocolContext, SupportedTransport, TransportManager, TransportManagerCommand,
            TransportManagerEvent,
        },
        types::protocol::ProtocolName,
        BandwidthSink,
    };
    use multihash::Multihash;
    use tokio::sync::mpsc::channel;

    #[test]
    fn parse_multiaddresses() {
        assert!(TcpTransport::get_socket_address(
            &"/ip6/::1/tcp/8888".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip6/::1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip6/::1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
        assert!(TcpTransport::get_socket_address(
            &"/ip4/127.0.0.1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
    }

    #[tokio::test]
    async fn connect_and_accept_works() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, _rx1) = channel(64);
        let (event_tx1, mut event_rx1) = channel(64);
        let (_cmd_tx1, cmd_rx1) = channel(64);
        let bandwidth_sink = BandwidthSink::new();

        let handle1 = crate::transport::manager::TransportHandle {
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair1.clone(),
            tx: event_tx1,
            rx: cmd_rx1,
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
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            ..Default::default()
        };

        let transport1 = TcpTransport::new(handle1, transport_config1).await.unwrap();

        let listen_address = Transport::listen_address(&transport1);
        tokio::spawn(async move {
            let _ = transport1.start().await;
        });

        let keypair2 = Keypair::generate();
        let (tx2, _rx2) = channel(64);
        let (event_tx2, mut event_rx2) = channel(64);
        let (cmd_tx2, cmd_rx2) = channel(64);

        let handle2 = crate::transport::manager::TransportHandle {
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair2.clone(),
            tx: event_tx2,
            rx: cmd_rx2,
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
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            ..Default::default()
        };

        let transport2 = TcpTransport::new(handle2, transport_config2).await.unwrap();

        tokio::spawn(async move {
            let _ = transport2.start().await;
        });

        let _peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public()));
        let _peer2: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public()));

        cmd_tx2
            .send(TransportManagerCommand::Dial {
                address: listen_address,
                connection: ConnectionId::new(),
            })
            .await
            .unwrap();

        let (res1, res2) = tokio::join!(event_rx1.recv(), event_rx2.recv());

        assert!(std::matches!(
            res1,
            Some(TransportManagerEvent::ConnectionEstablished { .. })
        ));
        assert!(std::matches!(
            res2,
            Some(TransportManagerEvent::ConnectionEstablished { .. })
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
        let (_cmd_tx1, cmd_rx1) = channel(64);
        let bandwidth_sink = BandwidthSink::new();

        let handle1 = crate::transport::manager::TransportHandle {
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair1.clone(),
            tx: event_tx1,
            rx: cmd_rx1,
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
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            ..Default::default()
        };

        let transport1 = TcpTransport::new(handle1, transport_config1).await.unwrap();

        tokio::spawn(transport1.start());

        let keypair2 = Keypair::generate();
        let (tx2, _rx2) = channel(64);
        let (event_tx2, mut event_rx2) = channel(64);
        let (cmd_tx2, cmd_rx2) = channel(64);

        let handle2 = crate::transport::manager::TransportHandle {
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair2.clone(),
            tx: event_tx2,
            rx: cmd_rx2,
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
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            ..Default::default()
        };

        let transport2 = TcpTransport::new(handle2, transport_config2).await.unwrap();
        tokio::spawn(transport2.start());

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

        cmd_tx2
            .send(TransportManagerCommand::Dial {
                address,
                connection: ConnectionId::new(),
            })
            .await
            .unwrap();

        // spawn the other conection in the background as it won't return anything
        tokio::spawn(async move {
            loop {
                let _ = event_rx1.recv().await;
            }
        });

        assert!(std::matches!(
            event_rx2.recv().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn dial_error_reported_for_outbound_connections() {
        let (mut manager, _handle) =
            TransportManager::new(Keypair::generate(), HashSet::new(), BandwidthSink::new());
        let handle = manager.register_transport(SupportedTransport::Tcp);
        let mut transport = TcpTransport::new(
            handle,
            TransportConfig {
                listen_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
                ..Default::default()
            },
        )
        .await
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

        match transport.on_dial_peer(multiaddr.clone(), ConnectionId::from(0usize)).await {
            Ok(()) => {}
            _ => panic!("invalid result for `on_dial_peer()`"),
        }

        assert!(!transport.pending_dials.is_empty());

        let _ = transport
            .on_connection_established(Err(TcpError {
                error: Error::Unknown,
                connection_id: Some(ConnectionId::from(0usize)),
            }))
            .await;

        assert!(transport.pending_dials.is_empty());
        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }
}
