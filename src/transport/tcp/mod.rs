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
// TODO: stop using println
// TODO: how to detect when substream is closed?
// TODO: think some more about all this boxing and `impl` madness
// TODO: ultimately transport would only yield connections and everything else would happen on upper layers

use crate::{
    config::{Role, TransportConfig},
    crypto::{
        ed25519::Keypair,
        noise::{self, NoiseConfiguration},
        PublicKey,
    },
    error::{AddressError, Error, SubstreamError},
    peer_id::PeerId,
    transport::{
        tcp::types::*, Connection, Direction, Transport, TransportEvent, TransportService,
    },
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
    sync::mpsc::{channel, Receiver, Sender},
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tracing::Level;

use std::{
    collections::HashMap,
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
};

mod types;

/// Logging target for the file.
const LOG_TARGET: &str = "transport::tcp";

/// TCP transport service.
pub struct TcpTransportService {
    /// TX channel for sending events to [`TcpTransport`].
    tx: Sender<TcpTransportEvent>,
}

impl TcpTransportService {
    /// Create new [`TcpTransportService`].
    fn new(tx: Sender<TcpTransportEvent>) -> Self {
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

    /// Open substream to remote peer.
    async fn open_substream(&mut self, protocol: String, peer: PeerId, handshake: Vec<u8>) {
        self.tx
            .send(TcpTransportEvent::OpenSubstream(protocol, peer, handshake))
            .await
            .expect("channel to `TcpTransport` to stay open")
    }
}

// TODO: documentation
struct MultiplexedConnection<S: Connection> {
    /// Peer ID of remote.
    peer: PeerId,

    /// Yamux connection.
    connection: yamux::ControlledConnection<S>,

    /// TX channel for sending negotiated substreams to [`TcpTransport`].
    tx: Sender<TransportEvent>,

    /// Supported protocols.
    protocols: Vec<String>,
}

impl<S: Connection> MultiplexedConnection<S> {
    /// Create new [`MultiplexedConnection`] and return a [`ConnectionContext`]
    /// that's linked to the connection.
    pub fn start(
        io: S,
        tx: Sender<TransportEvent>,
        peer: PeerId,
        role: Role,
        protocols: Vec<String>,
    ) -> ConnectionContext {
        let connection = yamux::Connection::new(io, yamux::Config::default(), role.into());
        let (control, mut connection) = yamux::Control::new(connection);
        let context = ConnectionContext::new(peer, control);

        let connection = Self {
            tx,
            peer,
            connection,
            protocols,
        };

        tokio::spawn(connection.run());
        context
    }

    /// Negotiate protocol.
    // TODO: move to some common place
    // TODO: this can't return impl connection, must be boxed?
    async fn negotiate_protocol(
        &mut self,
        io: impl Connection,
    ) -> crate::Result<(impl Connection, String)> {
        tracing::span!(target: LOG_TARGET, Level::TRACE, "negotiate protocol").enter();
        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            protocols = ?self.protocols,
            "negotiating protocols",
        );

        let (protocol, mut io) = listener_select_proto(io, &self.protocols).await?;

        tracing::event!(
            target: LOG_TARGET,
            Level::TRACE,
            ?protocol,
            "protocol negotiated",
        );

        Ok((io, protocol.to_owned()))
    }

    /// Run the [`MultiplexedConnection`] event loop.
    async fn run(mut self) {
        loop {
            while let Some(Ok(substream)) = self.connection.next().await {
                tracing::debug!(target: LOG_TARGET, peer = ?self.peer, "substream opened");

                let io = match self.negotiate_protocol(substream).await {
                    Ok((io, protocol)) => {
                        if let Err(_) = self
                            .tx
                            .send(TransportEvent::SubstreamOpened(
                                protocol,
                                self.peer,
                                Direction::Inbound,
                                Box::new(io),
                            ))
                            .await
                        {
                            tracing::error!(
                                target: LOG_TARGET,
                                "channel to `litep2p` closed, closing yamux connection"
                            );
                            return;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?error,
                            "failed to negotiate protocol for substream",
                        );
                        continue;
                    }
                };
            }
        }
    }
}

pub struct TcpTransport {
    /// TCP listener.
    listener: TcpListener,

    /// Keypair.
    keypair: Keypair,

    /// RX channel for receiving events from `litep2p`.
    // TODO: rename
    rx: Receiver<TcpTransportEvent>,

    /// TX channel for sending events to `litep2p`.
    tx: Sender<TransportEvent>,

    /// Pending outbound connections.
    pending_connections: PendingConnections,

    /// Pending outbound negotiations.
    pending_negotiations: PendingNegotiations,

    /// Incoming substreams.
    incoming_substreams: IncomingSubstreams,

    /// Pending outbound substreams.
    pending_outbound: PendingOutboundSubstreams,

    /// Open connections.
    connections: HashMap<PeerId, yamux::Control>,

    /// Supported protocols.
    protocols: Vec<String>,
}

impl TcpTransport {
    async fn new(
        keypair: &Keypair,
        config: TransportConfig,
        event_tx: Sender<TransportEvent>,
    ) -> crate::Result<(Self, Sender<TcpTransportEvent>)> {
        tracing::info!(target: LOG_TARGET, ?config, "create new `TcpTransport`");

        let (socket_address, _) = Self::get_socket_address(config.listen_address())?;
        let listener = TcpListener::bind(socket_address).await?;
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);
        let protocols = config.libp2p_protocols().cloned().collect();

        Ok((
            Self {
                listener,
                rx,
                tx: event_tx,
                keypair: keypair.clone(),
                protocols,
                pending_connections: FuturesUnordered::new(),
                pending_negotiations: FuturesUnordered::new(),
                incoming_substreams: FuturesUnordered::new(),
                pending_outbound: FuturesUnordered::new(),
                connections: HashMap::new(),
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
        io: impl Connection,
        role: &Role,
        protocols: Vec<&str>,
    ) -> crate::Result<impl Connection> {
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
        Ok(io)
    }

    /// Initialize connection.
    ///
    /// Negotiate and handshake Noise, negotiate Yamux and start a [`MultiplexedConnection`]
    /// event loop which listens to substream events, negotiates protocols for those substreams
    /// and returns the negotiated stream object back to [`TcpTransport`].
    async fn initialize_connection(
        io: impl Connection,
        tx: Sender<TransportEvent>,
        role: Role,
        noise_config: NoiseConfiguration,
        protocols: Vec<String>,
    ) -> crate::Result<ConnectionContext> {
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

        Ok(MultiplexedConnection::start(io, tx, peer, role, protocols))
    }

    /// Schedule connection negotiation.
    fn schedule_negotiation(&mut self, io: TcpStream, role: Role) {
        tracing::trace!(target: LOG_TARGET, ?role, "schedule negotiation");

        // create new Noise configuration and push a future which negotiates the connection
        let noise_config = NoiseConfiguration::new(&self.keypair, role);
        let protocols = self.protocols.iter().cloned().collect();
        let tx = self.tx.clone();

        self.pending_negotiations.push(Box::pin(async move {
            let io = TokioAsyncReadCompatExt::compat(io).into_inner();
            let io = Box::new(TokioAsyncWriteCompatExt::compat_write(io));
            Self::initialize_connection(io, tx, role, noise_config, protocols).await
        }));
    }

    /// Finalize the negotiated connection.
    ///
    // TODO: do something
    // TODO: pass connection context and not result
    fn on_negotiation_finished(&mut self, result: crate::Result<ConnectionContext>) {}

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

    /// Attempt to open substream over `protocol` to remote `peer`.
    // TODO: this code is so hideous
    // TODO: what to do what with handshake? Should it even be here?
    async fn on_open_substream(
        &mut self,
        protocol: String,
        peer: PeerId,
        handshake: Vec<u8>,
    ) -> crate::Result<()> {
        // TODO: clone only if necessary (optimally don't clone at all?)
        // let mut yamux = self
        //     .connections
        //     .get_mut(&peer)
        //     .ok_or(Error::SubstreamError(SubstreamError::ConnectionClosed))?
        //     .clone();

        let mut yamux = match self.connections.get_mut(&peer) {
            Some(yamux) => yamux.clone(),
            None => {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?peer,
                    ?protocol,
                    "connection does not exist"
                );
                return Err(Error::SubstreamError(SubstreamError::ConnectionClosed));
            }
        };

        tracing::info!(target: LOG_TARGET, ?peer, "here");

        self.pending_outbound.push(Box::pin(async move {
            let substream_result = match yamux.open_stream().await {
                Ok(substream) => {
                    // TODO: send handshake here???
                    Self::negotiate_protocol(substream, &Role::Dialer, vec![&protocol])
                        .await
                        .map(|io| -> Box<dyn Connection> { Box::new(io) })
                }
                Err(err) => Err(Error::SubstreamError(SubstreamError::YamuxError(err))),
            };
            (protocol, peer, substream_result)
        }));

        Ok(())
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
                    Ok((io, address)) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?address,
                            "inbound connection received",
                        );

                        self.schedule_negotiation(io, Role::Listener)
                    }
                },
                connection = self.pending_connections.select_next_some(), if !self.pending_connections.is_empty() => {
                    match connection {
                        Ok(io) => self.schedule_negotiation(io, Role::Dialer),
                        Err(error) => tracing::info!(
                            target: LOG_TARGET,
                            ?error,
                            "failed to establish outbound connection",
                        ),
                    }
                },
                negotiated = self.pending_negotiations.select_next_some(), if !self.pending_negotiations.is_empty() => {
                    match negotiated {
                        Ok(context) => {
                            tracing::trace!(
                                target: LOG_TARGET,
                                "negotiation finished"
                            );
                            self.connections.insert(context.peer, context.control);
                            // TODO: handle error
                            let _ = self.tx.send(TransportEvent::ConnectionEstablished(context.peer)).await;
                        }
                        Err(error) => {
                            tracing::error!(target: LOG_TARGET, ?error, "failed to negotiate connection")
                        }
                    }
                }
                outbound = self.pending_outbound.select_next_some(), if !self.pending_outbound.is_empty() => {
                    let _ = self.tx.send(match outbound.2 {
                        Ok(substream) => TransportEvent::SubstreamOpened(
                            outbound.0,
                            outbound.1,
                            Direction::Outbound,
                            substream,
                        ),
                        Err(err) => TransportEvent::SubstreamOpenFailure(
                            outbound.0,
                            outbound.1,
                            err,
                        ),
                    }).await;
                }
                event = self.rx.recv() => match event {
                    Some(TcpTransportEvent::OpenConnection(address)) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?address,
                            "establish outbound connection",
                        );

                        // TODO: verify that the number of connections is less than the specified limit
                        self.on_open_connection(address);
                    },
                    Some(TcpTransportEvent::CloseConnection(peer)) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            "close connection",
                        );

                        // TODO: emit connectionclosed?
                        self.connections.remove(&peer);
                    }
                    Some(TcpTransportEvent::OpenSubstream(protocol, peer, handshake)) => {
                        tracing::warn!(target: LOG_TARGET, ?peer, "here1");
                        if let Err(err) = self.on_open_substream(protocol.clone(), peer, handshake).await {
                            tracing::debug!(target: LOG_TARGET, ?peer, ?protocol, ?err, "failed to open substream");
                            let _ = self
                                .tx
                                .send(TransportEvent::SubstreamOpenFailure(protocol, peer, err))
                                .await;
                        }
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
    /// Start the underlying transport listener and return a handle which allows `litep2p` to
    // interact with the transport.
    // TODO: think about the design of `tx` some more
    async fn start(
        keypair: &Keypair,
        config: TransportConfig,
        tx: Sender<TransportEvent>,
    ) -> crate::Result<Box<dyn TransportService>> {
        let (transport, tx) = TcpTransport::new(keypair, config, tx).await?;

        tokio::spawn(transport.run());
        Ok(Box::new(TcpTransportService::new(tx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Libp2pProtocol, ProtocolName};

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
    #[ignore]
    async fn establish_outbound_connection() {
        // TODO: create listener as well
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair = Keypair::generate();
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);
        let mut handle = TcpTransport::start(
            &keypair,
            TransportConfig::new(
                "/ip6/::1/tcp/7777".parse().expect("valid multiaddress"),
                vec!["/ipfs/ping/1.0.0".to_owned()],
                vec![],
                40_000,
            ),
            tx,
        )
        .await
        .unwrap();

        // attempt to open connection to remote peer
        handle
            .open_connection("/ip6/::1/tcp/8888".parse().expect("valid multiaddress"))
            .await;

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;

        println!("exiting...");
    }
}
