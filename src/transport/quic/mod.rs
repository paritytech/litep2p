// Copyright 2021 Parity Technologies (UK) Ltd.
// Copyright 2022 Protocol Labs.
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

//! QUIC transport.

use crate::{
    crypto::tls::make_client_config,
    error::{AddressError, Error},
    transport::{
        manager::TransportHandle,
        quic::{config::TransportConfig as QuicTransportConfig, listener::QuicListener},
        Transport, TransportBuilder, TransportEvent,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use quinn::{ClientConfig, Connection, Endpoint};

use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

pub(crate) use substream::Substream;

mod connection;
mod listener;
mod substream;

pub mod config;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::quic";

#[derive(Debug)]
struct NegotiatedConnection {
    /// Remote peer ID.
    peer: PeerId,

    /// QUIC connection.
    connection: Connection,
}

/// QUIC transport object.
pub(crate) struct QuicTransport {
    /// Transport handle.
    context: TransportHandle,

    /// QUIC listener.
    listener: QuicListener,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections:
        FuturesUnordered<BoxFuture<'static, (ConnectionId, Result<NegotiatedConnection, Error>)>>,
}

impl QuicTransport {
    /// Attempt to extract `PeerId` from connection certificates.
    fn extract_peer_id(connection: &Connection) -> Option<PeerId> {
        let certificates: Box<Vec<rustls::Certificate>> =
            connection.peer_identity()?.downcast().ok()?;
        let p2p_cert = crate::crypto::tls::certificate::parse(certificates.get(0)?)
            .expect("the certificate was validated during TLS handshake; qed");

        Some(p2p_cert.peer_id())
    }

    /// Handle established connection.
    fn on_connection_established(
        &mut self,
        connection_id: ConnectionId,
        result: crate::Result<NegotiatedConnection>,
    ) -> Option<TransportEvent> {
        tracing::debug!(target: LOG_TARGET, ?connection_id, success = result.is_ok(), "connection established");

        // `on_connection_established()` is called for both inbound and outbound connections
        // but `pending_dials` will only contain entries for outbound connections.
        let maybe_address = self.pending_dials.remove(&connection_id);

        match result {
            Ok(connection) => {
                let endpoint = maybe_address.map_or(
                    {
                        let address = connection.connection.remote_address();
                        crate::transport::Endpoint::listener(
                            Multiaddr::empty()
                                .with(Protocol::from(address.ip()))
                                .with(Protocol::Udp(address.port()))
                                .with(Protocol::QuicV1),
                            connection_id,
                        )
                    },
                    |address| crate::transport::Endpoint::dialer(address, connection_id),
                );

                let bandwidth_sink = self.context.bandwidth_sink.clone();
                let mut protocol_set = self.context.protocol_set(connection_id);

                self.context.executor.run(Box::pin(async move {
                    let _ = protocol_set
                        .report_connection_established(connection_id, connection.peer, endpoint)
                        .await;

                    let _ = connection::Connection::new(
                        connection.peer,
                        connection_id,
                        connection.connection,
                        protocol_set,
                        bandwidth_sink,
                    )
                    .start()
                    .await;
                }));
            }
            Err(error) => {
                tracing::debug!(target: LOG_TARGET, ?connection_id, ?error, "failed to establish connection");

                // since the address was found from `pending_dials`,
                // report the error to protocols and `TransportManager`
                if let Some(address) = maybe_address {
                    return Some(TransportEvent::DialFailure {
                        connection_id,
                        address,
                        error,
                    });
                }
            }
        }

        None
    }
}

#[async_trait::async_trait]
impl TransportBuilder for QuicTransport {
    type Config = QuicTransportConfig;
    type Transport = QuicTransport;

    /// Create new [`QuicTransport`] object.
    async fn new(context: TransportHandle, config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_addresses = ?config.listen_addresses,
            "start quic transport",
        );

        let listener = QuicListener::new(&context.keypair, config.listen_addresses)?;

        Ok(Self {
            context,
            listener,
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Vec<Multiaddr> {
        self.listener.listen_addresses().cloned().collect()
    }

    async fn start(mut self) -> crate::Result<()> {
        while let Some(event) = self.next().await {
            match event {
                TransportEvent::ConnectionEstablished { .. } => {}
                TransportEvent::ConnectionClosed { .. } => {}
                TransportEvent::DialFailure {
                    connection_id,
                    address,
                    error,
                } => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?connection_id,
                        ?address,
                        ?error,
                        "failed to dial peer",
                    );

                    let _ = self.context.report_dial_failure(connection_id, address, error).await;
                }
            }
        }

        Ok(())
    }
}

impl Transport for QuicTransport {
    fn dial(&mut self, connection_id: ConnectionId, address: Multiaddr) -> crate::Result<()> {
        let Ok((socket_address, Some(peer))) = QuicListener::get_socket_address(&address) else {
            return Err(Error::AddressError(AddressError::PeerIdMissing));
        };

        let crypto_config =
            Arc::new(make_client_config(&self.context.keypair, Some(peer)).expect("to succeed"));
        let client_config = ClientConfig::new(crypto_config);
        let client_listen_address = match address.iter().next() {
            Some(Protocol::Ip6(_)) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
            Some(Protocol::Ip4(_)) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            _ => return Err(Error::AddressError(AddressError::InvalidProtocol)),
        };

        let client = Endpoint::client(client_listen_address)
            .map_err(|error| Error::Other(error.to_string()))?;
        let connection = client
            .connect_with(client_config, socket_address, "l")
            .map_err(|error| Error::Other(error.to_string()))?;

        tracing::trace!(
            target: LOG_TARGET,
            ?address,
            ?peer,
            ?client_listen_address,
            "dial peer",
        );

        self.pending_dials.insert(connection_id, address);
        self.pending_connections.push(Box::pin(async move {
            let connection = match connection.await {
                Ok(connection) => connection,
                Err(error) => return (connection_id, Err(error.into())),
            };

            let Some(peer) = Self::extract_peer_id(&connection) else {
                return (connection_id, Err(Error::InvalidCertificate));
            };

            (connection_id, Ok(NegotiatedConnection { peer, connection }))
        }));

        Ok(())
    }
}

impl Stream for QuicTransport {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(Some(connection)) = self.listener.poll_next_unpin(cx) {
            let connection_id = self.context.next_connection_id();

            tracing::trace!(
                target: LOG_TARGET,
                ?connection_id,
                "accept connection",
            );

            self.pending_connections.push(Box::pin(async move {
                let connection = match connection.await {
                    Ok(connection) => connection,
                    Err(error) => return (connection_id, Err(error.into())),
                };

                let Some(peer) = Self::extract_peer_id(&connection) else {
                    return (connection_id, Err(Error::InvalidCertificate));
                };

                (connection_id, Ok(NegotiatedConnection { peer, connection }))
            }));
        }

        while let Poll::Ready(Some(connection)) = self.pending_connections.poll_next_unpin(cx) {
            let (connection_id, result) = connection;

            match self.on_connection_established(connection_id, result) {
                None => {}
                Some(TransportEvent::DialFailure {
                    connection_id,
                    address,
                    error,
                }) => {
                    return Poll::Ready(Some(TransportEvent::DialFailure {
                        connection_id,
                        address,
                        error,
                    }));
                }
                _ => todo!(),
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
        transport::manager::{ProtocolContext, TransportHandle, TransportManagerEvent},
        types::protocol::ProtocolName,
        BandwidthSink,
    };
    use multihash::Multihash;
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn test_quinn() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair1 = Keypair::generate();
        let (tx1, _rx1) = channel(64);
        let (event_tx1, mut event_rx1) = channel(64);

        let handle1 = TransportHandle {
            executor: Arc::new(DefaultExecutor {}),
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair1.clone(),
            tx: event_tx1,
            bandwidth_sink: BandwidthSink::new(),

            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx1,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: Vec::new(),
                },
            )]),
        };
        let transport_config1 = QuicTransportConfig {
            listen_addresses: vec!["/ip6/::1/udp/0/quic-v1".parse().unwrap()],
        };

        let transport1 = QuicTransport::new(handle1, transport_config1).await.unwrap();

        let listen_address = TransportBuilder::listen_address(&transport1)[0].clone();

        tokio::spawn(async move {
            let _ = transport1.start().await;
        });

        let keypair2 = Keypair::generate();
        let (tx2, _rx2) = channel(64);
        let (event_tx2, mut event_rx2) = channel(64);

        let handle2 = TransportHandle {
            executor: Arc::new(DefaultExecutor {}),
            protocol_names: Vec::new(),
            next_substream_id: Default::default(),
            next_connection_id: Default::default(),
            keypair: keypair2.clone(),
            tx: event_tx2,
            bandwidth_sink: BandwidthSink::new(),

            protocols: HashMap::from_iter([(
                ProtocolName::from("/notif/1"),
                ProtocolContext {
                    tx: tx2,
                    codec: ProtocolCodec::Identity(32),
                    fallback_names: Vec::new(),
                },
            )]),
        };
        let transport_config2 = QuicTransportConfig {
            listen_addresses: vec!["/ip6/::1/udp/0/quic-v1".parse().unwrap()],
        };

        let mut transport2 = QuicTransport::new(handle2, transport_config2).await.unwrap();

        let peer1: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair1.public()));
        let _peer2: PeerId = PeerId::from_public_key(&PublicKey::Ed25519(keypair2.public()));
        let listen_address = listen_address.with(Protocol::P2p(
            Multihash::from_bytes(&peer1.to_bytes()).unwrap(),
        ));

        tokio::spawn(async move {
            transport2.dial(ConnectionId::new(), listen_address).unwrap();
            while let Some(_) = transport2.next().await {}
        });

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
}
