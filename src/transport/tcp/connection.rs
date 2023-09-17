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
    codec::{identity::Identity, unsigned_varint::UnsignedVarint, ProtocolCodec},
    config::Role,
    crypto::{
        ed25519::Keypair,
        noise::{self, NoiseSocket},
    },
    error::{Error, NegotiationError},
    multistream_select::{dialer_select_proto, listener_select_proto, Negotiated, Version},
    protocol::{Direction, Permit, ProtocolCommand, ProtocolSet},
    substream,
    transport::tcp::{substream::Substream, AddressType},
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId,
};

use futures::{
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
    AsyncRead, AsyncWrite,
};
use multiaddr::{Multiaddr, Protocol};
use tokio::net::TcpStream;
use tokio_util::{
    codec::Framed,
    compat::{
        Compat, FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt,
    },
};

use std::{borrow::Cow, fmt, net::SocketAddr, time::Duration};

// TODO: introduce `NegotiatingConnection` to clean up this code a bit?
/// Logging target for the file.
const LOG_TARGET: &str = "tcp::connection";

#[derive(Debug)]
pub struct NegotiatedSubstream {
    /// Substream direction.
    direction: Direction,

    /// Protocol name.
    protocol: ProtocolName,

    /// Yamux substream.
    io: yamux::Stream,

    /// Permit.
    permit: Permit,
}

/// TCP connection error.
#[derive(Debug)]
enum ConnectionError {
    /// Timeout
    Timeout {
        /// Protocol.
        protocol: Option<ProtocolName>,

        /// Substream ID.
        substream_id: Option<SubstreamId>,
    },

    /// Failed to negotiate connection/substream.
    FailedToNegotiate {
        /// Protocol.
        protocol: Option<ProtocolName>,

        /// Substream ID.
        substream_id: Option<SubstreamId>,

        /// Error.
        error: Error,
    },
}

/// TCP connection.
pub struct TcpConnection {
    /// Connection ID.
    connection_id: ConnectionId,

    /// Protocol context.
    protocol_set: ProtocolSet,

    /// Yamux connection.
    connection: yamux::ControlledConnection<NoiseSocket<Compat<TcpStream>>>,

    /// Yamux control.
    control: yamux::Control,

    /// Remote peer ID.
    peer: PeerId,

    /// Next substream ID.
    next_substream_id: SubstreamId,

    /// Remote address.
    address: Multiaddr,

    /// Pending substreams.
    pending_substreams:
        FuturesUnordered<BoxFuture<'static, Result<NegotiatedSubstream, ConnectionError>>>,
}

impl fmt::Debug for TcpConnection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TcpConnection")
            .field("peer", &self.peer)
            .field("next_substream_id", &self.next_substream_id)
            .finish()
    }
}

impl TcpConnection {
    /// Open connection to remote peer at `address`.
    pub(super) async fn open_connection(
        context: ProtocolSet,
        connection_id: ConnectionId,
        address: AddressType,
        peer: Option<PeerId>,
        yamux_config: yamux::Config,
        max_read_ahead_factor: usize,
        max_write_buffer_size: usize,
    ) -> crate::Result<Self> {
        tracing::debug!(
            target: LOG_TARGET,
            ?address,
            ?peer,
            "open connection to remote peer",
        );

        let keypair = context.keypair.clone();
        match tokio::time::timeout(std::time::Duration::from_secs(10), async move {
            let stream = match &address {
                AddressType::Socket(socket_address) => TcpStream::connect(socket_address).await?,
                AddressType::Dns(address, port) =>
                    TcpStream::connect(format!("{address}:{port}")).await?,
            };

            Self::negotiate_connection(
                stream,
                connection_id,
                context,
                keypair,
                Role::Dialer,
                address,
                yamux_config,
                max_read_ahead_factor,
                max_write_buffer_size,
            )
            .await
        })
        .await
        {
            Err(_) => Err(Error::Timeout),
            Ok(result) => result,
        }
    }

    /// Open substream for `protocol`.
    pub(super) async fn open_substream(
        mut control: yamux::Control,
        permit: Permit,
        direction: Direction,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
    ) -> crate::Result<NegotiatedSubstream> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?direction, "open substream");

        let stream = match control.open_stream().await {
            Ok(stream) => {
                tracing::trace!(target: LOG_TARGET, ?direction, "substream opened");
                stream
            }
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?direction,
                    ?error,
                    "failed to open substream"
                );
                return Err(Error::YamuxError(direction, error));
            }
        };

        // TODO: protocols don't change after they've been initialized so this should be done only
        // once
        let protocols = std::iter::once(&*protocol)
            .chain(fallback_names.iter().map(|protocol| &**protocol))
            .collect();

        let (io, protocol) = Self::negotiate_protocol(stream, &Role::Dialer, protocols).await?;

        Ok(NegotiatedSubstream {
            io: io.inner(),
            direction,
            protocol,
            permit,
        })
    }

    /// Accept a new connection.
    pub(super) async fn accept_connection(
        context: ProtocolSet,
        stream: TcpStream,
        connection_id: ConnectionId,
        address: SocketAddr,
        yamux_config: yamux::Config,
        max_read_ahead_factor: usize,
        max_write_buffer_size: usize,
    ) -> crate::Result<Self> {
        tracing::debug!(target: LOG_TARGET, ?address, "accept connection");

        let keypair = context.keypair.clone();
        match tokio::time::timeout(std::time::Duration::from_secs(10), async move {
            Self::negotiate_connection(
                stream,
                connection_id,
                context,
                // noise_config,
                keypair,
                Role::Listener,
                AddressType::Socket(address),
                yamux_config,
                max_read_ahead_factor,
                max_write_buffer_size,
            )
            .await
        })
        .await
        {
            Err(_) => return Err(Error::Timeout),
            Ok(result) => result,
        }
    }

    /// Accept substream.
    pub(super) async fn accept_substream(
        stream: yamux::Stream,
        permit: Permit,
        substream_id: SubstreamId,
        protocols: Vec<ProtocolName>,
    ) -> crate::Result<NegotiatedSubstream> {
        tracing::trace!(
            target: LOG_TARGET,
            ?substream_id,
            "accept inbound substream"
        );

        let protocols = protocols.iter().map(|protocol| &**protocol).collect::<Vec<&str>>();
        let (io, protocol) = Self::negotiate_protocol(stream, &Role::Listener, protocols).await?;

        tracing::trace!(
            target: LOG_TARGET,
            ?substream_id,
            "substream accepted and negotiated"
        );

        Ok(NegotiatedSubstream {
            io: io.inner(),
            direction: Direction::Inbound,
            protocol,
            permit,
        })
    }

    /// Negotiate protocol.
    async fn negotiate_protocol<S: AsyncRead + AsyncWrite + Unpin>(
        stream: S,
        role: &Role,
        protocols: Vec<&str>,
    ) -> crate::Result<(Negotiated<S>, ProtocolName)> {
        tracing::trace!(target: LOG_TARGET, ?protocols, "negotiating protocols");

        match tokio::time::timeout(Duration::from_secs(10), async move {
            match role {
                Role::Dialer => dialer_select_proto(stream, protocols, Version::V1).await,
                Role::Listener => listener_select_proto(stream, protocols).await,
            }
        })
        .await
        {
            Err(_) => Err(Error::Timeout),
            Ok(Err(error)) => Err(Error::NegotiationError(
                NegotiationError::MultistreamSelectError(error),
            )),
            Ok(Ok((protocol, socket))) => {
                tracing::trace!(target: LOG_TARGET, ?protocol, "protocol negotiated");

                Ok((socket, ProtocolName::from(protocol.to_string())))
            }
        }
    }

    /// Negotiate noise + yamux for the connection.
    async fn negotiate_connection(
        stream: TcpStream,
        connection_id: ConnectionId,
        mut protocol_set: ProtocolSet,
        keypair: Keypair,
        role: Role,
        address: AddressType,
        yamux_config: yamux::Config,
        max_read_ahead_factor: usize,
        max_write_buffer_size: usize,
    ) -> crate::Result<Self> {
        tracing::trace!(
            target: LOG_TARGET,
            ?role,
            "negotiate connection",
        );

        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);

        // negotiate `noise`
        let (stream, _) = Self::negotiate_protocol(stream, &role, vec!["/noise"]).await?;

        tracing::trace!(
            target: LOG_TARGET,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (stream, peer) = noise::handshake(
            stream.inner(),
            &keypair,
            role,
            max_read_ahead_factor,
            max_write_buffer_size,
        )
        .await?;
        tracing::trace!(target: LOG_TARGET, "noise handshake done");
        let stream: NoiseSocket<Compat<TcpStream>> = stream;

        // negotiate `yamux`
        let (stream, _) = Self::negotiate_protocol(stream, &role, vec!["/yamux/1.0.0"]).await?;
        tracing::trace!(target: LOG_TARGET, "`yamux` negotiated");

        let connection = yamux::Connection::new(stream.inner(), yamux_config, role.into());
        let (control, connection) = yamux::Control::new(connection);

        let address = match address {
            AddressType::Socket(address) => Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Tcp(address.port())),
            AddressType::Dns(address, port) => Multiaddr::empty()
                .with(Protocol::Dns(Cow::Owned(address)))
                .with(Protocol::Tcp(port)),
        };
        protocol_set
            .report_connection_established(connection_id, peer, address.clone())
            .await?;

        Ok(Self {
            peer,
            address,
            control,
            connection,
            protocol_set,
            connection_id,
            next_substream_id: SubstreamId::new(),
            pending_substreams: FuturesUnordered::new(),
        })
    }

    /// Get remote peer ID.
    pub(crate) fn peer(&self) -> &PeerId {
        &self.peer
    }

    /// Get remote address.
    pub(crate) fn address(&self) -> &Multiaddr {
        &self.address
    }

    /// Start connection event loop.
    pub(crate) async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                substream = self.connection.next() => match substream {
                    Some(Ok(stream)) => {
                        let substream = self.next_substream_id.next();
                        let protocols = self.protocol_set.protocols();
                        let permit = self.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5), // TODO: make this configurable
                                Self::accept_substream(stream, permit, substream, protocols),
                            )
                            .await
                            {
                                Ok(Ok(substream)) => Ok(substream),
                                Ok(Err(error)) => Err(ConnectionError::FailedToNegotiate {
                                    protocol: None,
                                    substream_id: None,
                                    error,
                                }),
                                Err(_) => Err(ConnectionError::Timeout {
                                    protocol: None,
                                    substream_id: None
                                }),
                            }
                        }));
                    },
                    Some(Err(error)) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            peer = ?self.peer,
                            ?error,
                            "connection closed with error"
                        );
                        self.protocol_set.report_connection_closed(self.peer, self.connection_id).await?;

                        return Ok(())
                    }
                    None => {
                        tracing::debug!(target: LOG_TARGET, peer = ?self.peer, "connection closed");
                        self.protocol_set.report_connection_closed(self.peer, self.connection_id).await?;

                        return Ok(())
                    }
                },
                // TODO: move this to a function
                substream = self.pending_substreams.select_next_some(), if !self.pending_substreams.is_empty() => {
                    match substream {
                        // TODO: return error to protocol
                        Err(error) => {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?error,
                                "failed to accept/open substream",
                            );

                            let (protocol, substream_id, error) = match error {
                                ConnectionError::Timeout { protocol, substream_id } => {
                                    (protocol, substream_id, Error::Timeout)
                                }
                                ConnectionError::FailedToNegotiate { protocol, substream_id, error } => {
                                    (protocol, substream_id, error)
                                }
                            };

                            match (protocol, substream_id) {
                                (Some(protocol), Some(substream_id)) => {
                                    if let Err(error) = self.protocol_set
                                        .report_substream_open_failure(protocol, substream_id, error)
                                        .await
                                    {
                                        tracing::error!(
                                            target: LOG_TARGET,
                                            ?error,
                                            "failed to register opened substream to protocol"
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                        Ok(substream) => {
                            let protocol = substream.protocol.clone();
                            let direction = substream.direction;
                            let socket = FuturesAsyncReadCompatExt::compat(substream.io);
                            let substream = substream::Substream::new_tcp(
                                self.peer,
                                Substream::new(socket, substream.permit),
                                self.protocol_set.protocol_codec(&protocol)
                            );

                            if let Err(error) = self.protocol_set
                                .report_substream_open(self.peer, protocol, direction, substream)
                                .await
                            {
                                tracing::error!(
                                    target: LOG_TARGET,
                                    ?error,
                                    "failed to register opened substream to protocol"
                                );
                            }
                        }
                    }
                }
                protocol = self.protocol_set.next_event() => match protocol {
                    Some(ProtocolCommand::OpenSubstream { protocol, fallback_names, substream_id, permit }) => {
                        let control = self.control.clone();

                        tracing::trace!(
                            target: LOG_TARGET,
                            ?protocol,
                            ?substream_id,
                            "open substream"
                        );

                        self.pending_substreams.push(Box::pin(async move {
                            match tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                Self::open_substream(control, permit, Direction::Outbound(substream_id), protocol.clone(), fallback_names),
                            )
                            .await
                            {
                                Ok(Ok(substream)) => Ok(substream),
                                Ok(Err(error)) => Err(ConnectionError::FailedToNegotiate {
                                    protocol: Some(protocol),
                                    substream_id: Some(substream_id),
                                    error,
                                }),
                                Err(_) => Err(ConnectionError::Timeout {
                                    protocol: Some(protocol),
                                    substream_id: Some(substream_id)
                                }),
                            }
                        }));
                    }
                    None => {
                        tracing::debug!(target: LOG_TARGET, "protocols have disconnected, closing connection");
                        return self.protocol_set.report_connection_closed(self.peer, self.connection_id).await
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        crypto::{ed25519::Keypair, PublicKey},
        transport::manager::{SupportedTransport, TransportManager, TransportManagerEvent},
    };
    use multihash::Multihash;
    use tokio::{io::AsyncWriteExt, net::TcpListener};

    #[tokio::test]
    async fn multistream_select_not_supported_dialer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        tokio::spawn(async move {
            match TcpConnection::open_connection(
                protocol_set,
                ConnectionId::from(0usize),
                AddressType::Socket(address),
                None,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let (mut stream, _) = listener.accept().await.unwrap();
        let _ = stream.write_all(&vec![0x12u8; 256]).await;

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn multistream_select_not_supported_listener() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        let (Ok(mut dialer), Ok((listener, dialer_address))) =
            tokio::join!(TcpStream::connect(address.clone()), listener.accept(),)
        else {
            panic!("failed to establish connection");
        };

        tokio::spawn(async move {
            match TcpConnection::accept_connection(
                protocol_set,
                listener,
                ConnectionId::from(0usize),
                dialer_address,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let _ = dialer.write_all(&vec![0x12u8; 256]).await;

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn noise_not_supported_dialer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        tokio::spawn(async move {
            match TcpConnection::open_connection(
                protocol_set,
                ConnectionId::from(0usize),
                AddressType::Socket(address),
                None,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let (stream, _) = listener.accept().await.unwrap();
        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);

        // attempt to negotiate yamux, skipping noise entirely
        assert!(listener_select_proto(stream, vec!["/yamux/1.0.0"]).await.is_err());

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn noise_not_supported_listener() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        let (Ok(dialer), Ok((listener, dialer_address))) =
            tokio::join!(TcpStream::connect(address.clone()), listener.accept(),)
        else {
            panic!("failed to establish connection");
        };

        tokio::spawn(async move {
            match TcpConnection::accept_connection(
                protocol_set,
                listener,
                ConnectionId::from(0usize),
                dialer_address,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let dialer = TokioAsyncReadCompatExt::compat(dialer).into_inner();
        let dialer = TokioAsyncWriteCompatExt::compat_write(dialer);

        // attempt to negotiate yamux, skipping noise entirely
        assert!(dialer_select_proto(dialer, vec!["/yamux/1.0.0"], Version::V1).await.is_err());

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn noise_timeout_dialer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        let (Ok(dialer), Ok((listener, dialer_address))) =
            tokio::join!(TcpStream::connect(address.clone()), listener.accept(),)
        else {
            panic!("failed to establish connection");
        };

        tokio::spawn(async move {
            match TcpConnection::accept_connection(
                protocol_set,
                listener,
                ConnectionId::from(0usize),
                dialer_address,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let dialer = TokioAsyncReadCompatExt::compat(dialer).into_inner();
        let dialer = TokioAsyncWriteCompatExt::compat_write(dialer);

        // attempt to negotiate yamux, skipping noise entirely
        assert!(dialer_select_proto(dialer, vec!["/noise"], Version::V1).await.is_ok());

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn noise_timeout_listener() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        tokio::spawn(async move {
            match TcpConnection::open_connection(
                protocol_set,
                ConnectionId::from(0usize),
                AddressType::Socket(address),
                None,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let (stream, _) = listener.accept().await.unwrap();
        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);

        // negotiate noise but never actually send any handshake data
        assert!(listener_select_proto(stream, vec!["/noise"]).await.is_ok());

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn multistream_select_timeout_dialer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        tokio::spawn(async move {
            match TcpConnection::open_connection(
                protocol_set,
                ConnectionId::from(0usize),
                AddressType::Socket(address),
                None,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn multistream_select_timeout_listener() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        let (Ok(_dialer), Ok((listener, dialer_address))) =
            tokio::join!(TcpStream::connect(address.clone()), listener.accept(),)
        else {
            panic!("failed to establish connection");
        };

        tokio::spawn(async move {
            match TcpConnection::accept_connection(
                protocol_set,
                listener,
                ConnectionId::from(0usize),
                dialer_address,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn yamux_not_supported_dialer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        let (Ok(dialer), Ok((listener, dialer_address))) =
            tokio::join!(TcpStream::connect(address.clone()), listener.accept(),)
        else {
            panic!("failed to establish connection");
        };

        tokio::spawn(async move {
            match TcpConnection::accept_connection(
                protocol_set,
                listener,
                ConnectionId::from(0usize),
                dialer_address,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let dialer = TokioAsyncReadCompatExt::compat(dialer).into_inner();
        let dialer = TokioAsyncWriteCompatExt::compat_write(dialer);

        // negotiate noise
        let (_protocol, stream) =
            dialer_select_proto(dialer, vec!["/noise"], Version::V1).await.unwrap();

        let keypair = Keypair::generate();
        // let noise_config = NoiseConfiguration::new(&keypair, Role::Dialer, 5, 2);

        // do a noise handshake
        let (stream, _peer) =
            noise::handshake(stream.inner(), &keypair, Role::Dialer, 5, 2).await.unwrap();
        let stream: NoiseSocket<Compat<TcpStream>> = stream;

        // after the handshake, try to negotiate some random protocol instead of yamux
        assert!(dialer_select_proto(stream, vec!["/unsupported/1"], Version::V1).await.is_err());

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn yamux_not_supported_listener() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        tokio::spawn(async move {
            match TcpConnection::open_connection(
                protocol_set,
                ConnectionId::from(0usize),
                AddressType::Socket(address),
                None,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let (stream, _) = listener.accept().await.unwrap();
        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);

        // negotiate noise
        let (_protocol, stream) = listener_select_proto(stream, vec!["/noise"]).await.unwrap();

        // do a noise handshake
        let keypair = Keypair::generate();
        let (stream, _peer) =
            noise::handshake(stream.inner(), &keypair, Role::Listener, 5, 2).await.unwrap();
        let stream: NoiseSocket<Compat<TcpStream>> = stream;

        // after the handshake, try to negotiate some random protocol instead of yamux
        assert!(listener_select_proto(stream, vec!["/unsupported/1"]).await.is_err());

        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn yamux_timeout_dialer() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        let (Ok(dialer), Ok((listener, dialer_address))) =
            tokio::join!(TcpStream::connect(address.clone()), listener.accept(),)
        else {
            panic!("failed to establish connection");
        };

        tokio::spawn(async move {
            match TcpConnection::accept_connection(
                protocol_set,
                listener,
                ConnectionId::from(0usize),
                dialer_address,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let dialer = TokioAsyncReadCompatExt::compat(dialer).into_inner();
        let dialer = TokioAsyncWriteCompatExt::compat_write(dialer);

        // negotiate noise
        let (_protocol, stream) =
            dialer_select_proto(dialer, vec!["/noise"], Version::V1).await.unwrap();

        // do a noise handshake
        let keypair = Keypair::generate();
        let (stream, _peer) =
            noise::handshake(stream.inner(), &keypair, Role::Dialer, 5, 2).await.unwrap();
        let _stream: NoiseSocket<Compat<TcpStream>> = stream;

        // after noise handshake, don't negotiate anything and wait for the substream to time out
        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }

    #[tokio::test]
    async fn yamux_timeout_listener() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let listener = TcpListener::bind("[::1]:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let keypair = Keypair::generate();
        let peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));
        let multiaddr = Multiaddr::empty()
            .with(Protocol::from(address.ip()))
            .with(Protocol::Tcp(address.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer_id.to_bytes()).unwrap(),
            ));
        let (mut manager, _handle) = TransportManager::new(keypair);

        let _service = manager.register_protocol(
            ProtocolName::from("/notif/1"),
            Vec::new(),
            ProtocolCodec::UnsignedVarint(None),
        );
        let mut handle = manager.register_transport(SupportedTransport::Tcp);
        let protocol_set = handle.protocol_set();
        let _ = manager.dial_address(multiaddr.clone()).await;
        let _ = handle.next().await.unwrap();

        tokio::spawn(async move {
            match TcpConnection::open_connection(
                protocol_set,
                ConnectionId::from(0usize),
                AddressType::Socket(address),
                None,
                Default::default(),
                5,
                2,
            )
            .await
            {
                Ok(_) => panic!("connection was supposed to fail"),
                Err(error) =>
                    handle.report_dial_failure(ConnectionId::from(0usize), multiaddr, error).await,
            }
        });

        let (stream, _) = listener.accept().await.unwrap();
        let stream = TokioAsyncReadCompatExt::compat(stream).into_inner();
        let stream = TokioAsyncWriteCompatExt::compat_write(stream);

        // negotiate noise
        let (_protocol, stream) = listener_select_proto(stream, vec!["/noise"]).await.unwrap();

        // do a noise handshake
        let keypair = Keypair::generate();
        let (stream, _peer) =
            noise::handshake(stream.inner(), &keypair, Role::Listener, 5, 2).await.unwrap();
        let _stream: NoiseSocket<Compat<TcpStream>> = stream;

        // after noise handshake, don't negotiate anything and wait for the substream to time out
        assert!(std::matches!(
            manager.next().await,
            Some(TransportManagerEvent::DialFailure { .. })
        ));
    }
}
