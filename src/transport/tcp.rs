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

//! TCP transport implementation.

// TODO: remove excess boxing
// TODO: support two connections per peer
// TODO: move code to `transport/tcp/types.rs`
// TODO: bechmark `StreamMap` performance under high load
// TODO: find a better way to

use crate::{
    config::{Role, TransportConfig},
    crypto::{
        ed25519,
        noise::{self, NoiseConfiguration},
        PublicKey,
    },
    error::{AddressError, Error},
    peer_id::PeerId,
    transport::{Connection, ConnectionContext, Transport, TransportEvent, TransportService},
    types::{ProtocolId, ProtocolType, RequestId, SubstreamId},
    DEFAULT_CHANNEL_SIZE,
};

use futures::{
    io::{AsyncRead, AsyncWrite},
    stream::FuturesUnordered,
    FutureExt, Stream, StreamExt,
};
use multiaddr::{Multiaddr, Protocol};
use multistream_select::{dialer_select_proto, listener_select_proto, Version};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};
use tokio_stream::StreamMap;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::Level;

use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

/// Logging target for the file.
const LOG_TARGET: &str = "transport::tcp";

/// Type representing pending outbound connections.
type PendingConnections =
    FuturesUnordered<Pin<Box<dyn Future<Output = Result<TcpStream, std::io::Error>> + Send>>>;

/// Type representing pending negotiations.
type PendingNegotiations = FuturesUnordered<
    Pin<
        Box<
            dyn Future<Output = crate::Result<(yamux::Connection<Box<dyn Connection>>, PeerId)>>
                + Send,
        >,
    >,
>;

/// TCP transport events.
#[derive(Debug)]
enum TcpTransportEvent {
    /// Open connection to remote peer.
    OpenConnection(Multiaddr),

    /// Close connection to remote peer.
    CloseConnection(PeerId),
}

/// TCP transport service.
pub struct TcpTransportService {
    /// TX channel for sending events to [`TcpTransport`].
    tx: mpsc::Sender<TcpTransportEvent>,
}

impl TcpTransportService {
    /// Create new [`TcpTransportService`].
    fn new(tx: mpsc::Sender<TcpTransportEvent>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl TransportService for TcpTransportService {
    /// Open connection to remote peer.
    async fn open_connection(&mut self, address: Multiaddr) {
        self.tx
            .send(TcpTransportEvent::OpenConnection(address))
            .await
            .expect("channel to `TcpTransport` to stay open")
    }

    /// Instruct [`TcpTransport`] to close connection to remote peer.
    async fn close_connection(&mut self, peer: PeerId) {
        self.tx
            .send(TcpTransportEvent::CloseConnection(peer))
            .await
            .expect("channel to `TcpTransport` to stay open")
    }
}

pub struct TcpTransport {
    /// TCP listener.
    listener: TcpListener,

    /// RX channel for receiving events from `litep2p`.
    rx: mpsc::Receiver<TcpTransportEvent>,

    /// Pending outbound connections.
    pending_connections: PendingConnections,

    /// Pending outbound negotiations.
    pending_negotiations: PendingNegotiations,

    /// Open connections.
    connections: StreamMap<PeerId, yamux::ControlledConnection<Box<dyn Connection>>>,
}

impl TcpTransport {
    async fn new(
        config: TransportConfig,
    ) -> crate::Result<(Self, mpsc::Sender<TcpTransportEvent>)> {
        tracing::info!(target: LOG_TARGET, ?config, "create new `TcpTransport`");

        let (socket_address, _) = Self::get_socket_address(config.listen_address())?;
        let listener = TcpListener::bind(socket_address).await?;
        let (tx, rx) = mpsc::channel(DEFAULT_CHANNEL_SIZE);

        Ok((
            Self {
                listener,
                rx,
                pending_connections: FuturesUnordered::new(),
                pending_negotiations: FuturesUnordered::new(),
                connections: StreamMap::new(),
            },
            tx,
        ))
    }
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

    /// Negotiate protocol.
    async fn negotiate_protocol(
        io: Box<dyn Connection>,
        role: &Role,
        protocols: Vec<&str>,
    ) -> crate::Result<Box<dyn Connection>> {
        tracing::span!(target: LOG_TARGET, Level::TRACE, "negotiate protocol").enter();
        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            ?protocols,
            "negotiating protocols",
        );

        let (protocol, mut io) = match role {
            Role::Dialer => dialer_select_proto(io, protocols, Version::V1).await?,
            Role::Listener => listener_select_proto(io, protocols).await?,
        };

        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            ?protocol,
            "protocol negotiated",
        );

        // TODO: return selected protocol?
        Ok(Box::new(io))
    }

    /// Initialize connection.
    ///
    /// Negotiate and handshake Noise and Yamux.
    async fn initialize_connection(
        io: Box<dyn Connection>,
        role: Role,
        noise_config: NoiseConfiguration,
    ) -> crate::Result<(yamux::Connection<Box<dyn Connection>>, PeerId)> {
        tracing::span!(target: LOG_TARGET, Level::DEBUG, "negotiate connection").enter();
        tracing::event!(
            target: LOG_TARGET,
            Level::DEBUG,
            ?role,
            "negotiate connection",
        );

        // negotiate `noise`
        let io = Self::negotiate_protocol(io, &role, vec!["/noise"]).await?;
        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            "`multistream-select` and `noise` negotiated"
        );

        // perform noise handshake
        let (io, peer) = noise::handshake(io, noise_config).await?;
        tracing::event!(target: LOG_TARGET, Level::TRACE, "noise handshake done");

        // negotiate `yamux`
        let io = Self::negotiate_protocol(io, &role, vec!["/yamux/1.0.0"]).await?;
        tracing::event!(target: LOG_TARGET, Level::TRACE, "`yamux` negotiated");

        Ok((
            yamux::Connection::new(io, yamux::Config::default(), role.into()),
            peer,
        ))

        // todo!();
        // while let Some(event) = connection.next().await {
        //     match event {
        //         Ok(mut substream) => {
        //             tokio::spawn(async move {
        //                 // TODO: add all supported protocols.
        //                 let protos = Vec::from(["/ipfs/ping/1.0.0"]);
        //                 let (protocol, mut socket) =
        //                     listener_select_proto(substream, protos).await.unwrap();

        //                 // TODO: start correct protocol handler based on the value of `protocol`
        //                 println!("selected protocol {protocol:?}");

        //                 // TODO: answer to pings
        //                 tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        //             });
        //         }
        //         Err(err) => {
        //             println!("failed to receive inbound substream: {err:?}");
        //         }
        //     }
        // }

        // // TODO: maybe don't return connection context but save it to `TransportService`?
        // Ok(ConnectionContext {
        //     io: Box::new(io),
        //     peer,
        // })
    }

    /// Schedule connection negotiation.
    fn schedule_negotiation(&mut self, io: TcpStream, role: Role) {
        tracing::trace!(target: LOG_TARGET, ?role, "schedule negotiation");

        let noise_config = todo!();

        self.pending_negotiations.push(Box::pin(async move {
            let io = TokioAsyncReadCompatExt::compat(io).into_inner();
            let io = Box::new(TokioAsyncWriteCompatExt::compat_write(io));
            Self::initialize_connection(io, role, noise_config).await
        }));
    }

    /// Finalize the negotiated connection.
    ///
    /// TODO: do something
    fn on_negotiation_finished(
        &mut self,
        result: crate::Result<(yamux::Connection<Box<dyn Connection>>, PeerId)>,
    ) {
        match result {
            Ok((connection, peer)) => {
                let (mut control, mut connection) = yamux::Control::new(connection);
                self.connections.insert(peer, connection);
            }
            Err(error) => {
                tracing::error!(target: LOG_TARGET, ?error, "failed to negotiate connection");
            }
        }
    }

    /// Handle `TcpTransportEvent::OpenConnection`.
    ///
    /// Parse the received `Multiaddr` and if it contains a valid address understood by [`TcpTransport`],
    /// create a future which attempts to establish a connection with the specified remote peer.
    fn on_open_connection(&mut self, address: Multiaddr) {
        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            ?address,
            "attempt to establish outbound connections",
        );

        let (socket_address, peer) = match Self::get_socket_address(&address) {
            Ok((address, peer)) => (address, peer),
            Err(error) => {
                tracing::error!(target: LOG_TARGET, ?error, "failed to parse `Multiaddr`");
                return;
            }
        };

        self.pending_connections.push(Box::pin(
            async move { TcpStream::connect(socket_address).await },
        ));
    }

    /// Run the [`TcpTransport`] event loop.
    async fn run(mut self) {
        tracing::info!(target: LOG_TARGET, "starting `TcpTransport` event loop");

        loop {
            tokio::select! {
                event = self.listener.accept() => match event {
                    Err(error) => {
                        tracing::error!(
                            target: LOG_TARGET,
                            ?error,
                            "listener failed",
                        );
                        return
                    }
                    Ok((io, _address)) => self.schedule_negotiation(io, Role::Listener),
                },
                connection = self.pending_connections.select_next_some() => match connection {
                    Ok(io) => self.schedule_negotiation(io, Role::Dialer),
                    Err(error) => tracing::info!(
                        target: LOG_TARGET,
                        ?error,
                        "failed to establish outbound connection",
                    ),
                },
                negotiated = self.pending_negotiations.select_next_some() => {
                    self.on_negotiation_finished(negotiated);
                }
                event = self.connections.next() => match event {
                    Some((peer, Ok(stream))) => {
                        todo!();
                    }
                    Some((peer, Err(error))) => {
                        tracing::error!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to poll yamux connection"
                        );
                        self.connections.remove(&peer);
                    }
                    None => {
                        todo!();
                    }
                },
                event = self.rx.recv() => match event {
                    Some(TcpTransportEvent::OpenConnection(address)) => {
                        self.on_open_connection(address);
                    },
                    Some(TcpTransportEvent::CloseConnection(peer)) => {
                        self.connections.remove(&peer);
                    }
                    None => {
                        tracing::error!(
                            target: LOG_TARGET,
                            "`TcpTransportEvent` TX channel closed, closing `TcpTransport`",
                        );
                        return
                    }
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl Transport for TcpTransport {
    type Handle = TcpTransportService;

    /// Start the underlying transport listener and return a handle which allows `litep2p` to
    // interact with the transport.
    async fn start(config: TransportConfig) -> crate::Result<Self::Handle> {
        let (transport, tx) = TcpTransport::new(config).await?;

        tokio::spawn(transport.run());
        Ok(TcpTransportService::new(tx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_multiaddresses() {
        assert!(TcpTransport::get_socket_address(
            &"/ip6/::1/tcp/8888".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpTransport::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888"
                .parse()
                .expect("valid multiaddress")
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
    async fn establish_outbound_connection() {
        // TODO: create listener as well
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init()
            .expect("to succeed");

        let mut handle = TcpTransport::start(TransportConfig::new(
            "/ip6/::1/tcp/7777".parse().expect("valid multiaddress"),
            vec![],
            40_000,
        ))
        .await
        .unwrap();

        // attempt to open connection to remote peer
        handle
            .open_connection("/ip6/::1/tcp/8888".parse().expect("valid multiaddress"))
            .await;

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;

        // let mut transport = TcpTransportService::new();
        // let keypair = ed25519::Keypair::generate();
        // let config = NoiseConfiguration::new(&keypair, crate::config::Role::Dialer);

        // transport
        //     .open_connection(
        //         "/ip6/::1/tcp/8888".parse().expect("valid multiaddress"),
        //         config,
        //     )
        //     .await
        //     .unwrap();
    }
}
