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
    crypto::{ed25519::Keypair, tls::make_server_config},
    error::AddressError,
    transport::common::listener::{AddressType, DnsType},
    PeerId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use quinn::{crypto::rustls::QuicServerConfig, Connecting, Endpoint, ServerConfig};

use std::{
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::quic::listener";

/// Await the next inbound connection on `listener`.
///
/// quinn 0.11 surfaces inbound connections as [`quinn::Incoming`], which must be explicitly
/// accepted before it becomes a [`Connecting`]. We accept every incoming connection, skipping the
/// ones that fail to be accepted (e.g. the peer went away), and resolve to `None` once the
/// endpoint is closed.
async fn accept_connection(index: usize, listener: Endpoint) -> Option<(usize, Connecting)> {
    loop {
        let incoming = listener.accept().await?;

        match incoming.accept() {
            Ok(connecting) => return Some((index, connecting)),
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?error,
                    "failed to accept inbound quic connection",
                );
            }
        }
    }
}

/// QUIC listener.
pub struct QuicListener {
    /// Listen addresses.
    _listen_addresses: Vec<SocketAddr>,

    /// Listeners.
    listeners: Vec<Endpoint>,

    /// Incoming connections.
    incoming: FuturesUnordered<BoxFuture<'static, Option<(usize, Connecting)>>>,
}

impl QuicListener {
    /// Create new [`QuicListener`].
    pub fn new(
        keypair: &Keypair,
        addresses: Vec<Multiaddr>,
    ) -> crate::Result<(Self, Vec<Multiaddr>)> {
        let (listeners, listen_addresses): (Vec<Endpoint>, Vec<SocketAddr>) = addresses
            .into_iter()
            .filter_map(|address| {
                let bind_address = match Self::get_socket_address(&address).ok()?.0 {
                    AddressType::Socket(address) => address,
                    AddressType::Dns { address, port, .. } => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?address,
                            ?port,
                            "dns not supported as bind address",
                        );
                        return None;
                    }
                };
                let crypto_config = make_server_config(keypair).expect("to succeed");
                let quic_crypto = Arc::new(
                    QuicServerConfig::try_from(crypto_config)
                        .expect("TLS config to be valid for QUIC; qed"),
                );
                let server_config = ServerConfig::with_crypto(quic_crypto);
                let listener = match Endpoint::server(server_config, bind_address) {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?address,
                            ?error,
                            "failed to bind quic listener",
                        );
                        return None;
                    }
                };
                let local_address = listener.local_addr().ok()?;
                Some((listener, local_address))
            })
            .unzip();

        let listen_multi_addresses = listen_addresses
            .iter()
            .cloned()
            .map(|address| {
                Multiaddr::empty()
                    .with(Protocol::from(address.ip()))
                    .with(Protocol::Udp(address.port()))
                    .with(Protocol::QuicV1)
            })
            .collect();

        Ok((
            Self {
                incoming: listeners
                    .iter()
                    .enumerate()
                    .map(|(i, listener)| accept_connection(i, listener.clone()).boxed())
                    .collect(),
                listeners,
                _listen_addresses: listen_addresses,
            },
            listen_multi_addresses,
        ))
    }

    /// Parse a QUIC multiaddress, supporting both IP and DNS addresses.
    ///
    /// Accepted formats:
    /// - `/ip4/<addr>/udp/<port>/quic-v1[/p2p/<peer-id>]`
    /// - `/ip6/<addr>/udp/<port>/quic-v1[/p2p/<peer-id>]`
    /// - `/dns/<host>/udp/<port>/quic-v1[/p2p/<peer-id>]`
    /// - `/dns4/<host>/udp/<port>/quic-v1[/p2p/<peer-id>]`
    /// - `/dns6/<host>/udp/<port>/quic-v1[/p2p/<peer-id>]`
    pub fn get_socket_address(
        address: &Multiaddr,
    ) -> Result<(AddressType, Option<PeerId>), AddressError> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse quic multi address");

        let mut iter = address.iter();
        let handle_dns_type =
            |address: String, dns_type: DnsType, protocol: Option<Protocol>| match protocol {
                Some(Protocol::Udp(port)) => Ok(AddressType::Dns {
                    address,
                    port,
                    dns_type,
                }),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Udp`",
                    );
                    Err(AddressError::InvalidProtocol)
                }
            };

        let address_type = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Udp(port)) =>
                    AddressType::Socket(SocketAddr::new(IpAddr::V6(address), port)),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Udp`",
                    );
                    return Err(AddressError::InvalidProtocol);
                }
            },
            Some(Protocol::Ip4(address)) => match iter.next() {
                Some(Protocol::Udp(port)) =>
                    AddressType::Socket(SocketAddr::new(IpAddr::V4(address), port)),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Udp`",
                    );
                    return Err(AddressError::InvalidProtocol);
                }
            },
            Some(Protocol::Dns(address)) =>
                handle_dns_type(address.into(), DnsType::Dns, iter.next())?,
            Some(Protocol::Dns4(address)) =>
                handle_dns_type(address.into(), DnsType::Dns4, iter.next())?,
            Some(Protocol::Dns6(address)) =>
                handle_dns_type(address.into(), DnsType::Dns6, iter.next())?,
            protocol => {
                tracing::error!(target: LOG_TARGET, ?protocol, "invalid transport protocol");
                return Err(AddressError::InvalidProtocol);
            }
        };

        // verify that quic exists
        match iter.next() {
            Some(Protocol::QuicV1) => {}
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `QuicV1`",
                );
                return Err(AddressError::InvalidProtocol);
            }
        }

        let maybe_peer = match iter.next() {
            Some(Protocol::P2p(multihash)) =>
                Some(PeerId::from_multihash(multihash).map_err(AddressError::InvalidPeerId)?),
            None => None,
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `P2p` or `None`",
                );
                return Err(AddressError::InvalidProtocol);
            }
        };

        Ok((address_type, maybe_peer))
    }
}

impl Stream for QuicListener {
    type Item = Connecting;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.incoming.is_empty() {
            return Poll::Pending;
        }

        match futures::ready!(self.incoming.poll_next_unpin(cx)) {
            None => Poll::Ready(None),
            Some(None) => Poll::Ready(None),
            Some(Some((listener, future))) => {
                let inner = self.listeners[listener].clone();
                self.incoming.push(accept_connection(listener, inner).boxed());

                Poll::Ready(Some(future))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::crypto::tls::make_client_config;

    use super::*;
    use quinn::{crypto::rustls::QuicClientConfig, ClientConfig};
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    fn peer_id() -> PeerId {
        PeerId::from_public_key(&Keypair::generate().public().into())
    }

    #[test]
    fn parse_multiaddresses() {
        let peer = peer_id();

        // IPv4 with peer ID
        let address: Multiaddr =
            format!("/ip4/192.168.1.1/udp/5000/quic-v1/p2p/{peer}").parse().unwrap();
        let (address_type, parsed_peer) = QuicListener::get_socket_address(&address).unwrap();
        assert!(
            matches!(address_type, AddressType::Socket(addr) if addr.port() == 5000 && addr.ip().to_string() == "192.168.1.1")
        );
        assert_eq!(parsed_peer, Some(peer));

        // IPv6 with peer ID
        let address: Multiaddr = format!("/ip6/::1/udp/9000/quic-v1/p2p/{peer}").parse().unwrap();
        let (address_type, parsed_peer) = QuicListener::get_socket_address(&address).unwrap();
        assert!(matches!(address_type, AddressType::Socket(addr) if addr.port() == 9000));
        assert_eq!(parsed_peer, Some(peer));

        // DNS with peer ID
        let address: Multiaddr =
            format!("/dns/example.com/udp/5000/quic-v1/p2p/{peer}").parse().unwrap();
        let (address_type, parsed_peer) = QuicListener::get_socket_address(&address).unwrap();
        assert!(matches!(
            address_type,
            AddressType::Dns { ref address, port: 5000, dns_type: DnsType::Dns }
            if address == "example.com"
        ));
        assert_eq!(parsed_peer, Some(peer));

        // DNS4 with peer ID
        let address: Multiaddr =
            format!("/dns4/example.com/udp/8080/quic-v1/p2p/{peer}").parse().unwrap();
        let (address_type, parsed_peer) = QuicListener::get_socket_address(&address).unwrap();
        assert!(matches!(
            address_type,
            AddressType::Dns { ref address, port: 8080, dns_type: DnsType::Dns4 }
            if address == "example.com"
        ));
        assert_eq!(parsed_peer, Some(peer));

        // DNS6 with peer ID
        let address: Multiaddr =
            format!("/dns6/example.com/udp/3000/quic-v1/p2p/{peer}").parse().unwrap();
        let (address_type, parsed_peer) = QuicListener::get_socket_address(&address).unwrap();
        assert!(matches!(
            address_type,
            AddressType::Dns { ref address, port: 3000, dns_type: DnsType::Dns6 }
            if address == "example.com"
        ));
        assert_eq!(parsed_peer, Some(peer));

        // Without peer ID
        let address: Multiaddr = "/ip4/192.168.1.1/udp/5000/quic-v1".parse().unwrap();
        let (address_type, parsed_peer) = QuicListener::get_socket_address(&address).unwrap();
        assert!(matches!(address_type, AddressType::Socket(_)));
        assert_eq!(parsed_peer, None);

        // Plain IP variants without peer ID still parse.
        assert!(QuicListener::get_socket_address(
            &"/ip6/::1/udp/8888/quic-v1".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(QuicListener::get_socket_address(
            &"/ip4/127.0.0.1/udp/8888/quic-v1".parse().expect("valid multiaddress")
        )
        .is_ok());

        // Invalid: TCP after IP instead of UDP
        assert!(matches!(
            QuicListener::get_socket_address(
                &"/ip6/::1/tcp/8888/quic-v1/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                    .parse()
                    .expect("valid multiaddress")
            ),
            Err(AddressError::InvalidProtocol)
        ));

        // Invalid: missing quic-v1
        assert!(matches!(
            QuicListener::get_socket_address(
                &"/ip4/127.0.0.1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                    .parse()
                    .expect("valid multiaddress")
            ),
            Err(AddressError::InvalidProtocol)
        ));

        // Invalid: TCP after IP, no quic-v1
        assert!(matches!(
            QuicListener::get_socket_address(
                &"/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                    .parse()
                    .expect("valid multiaddress")
            ),
            Err(AddressError::InvalidProtocol)
        ));

        // Invalid: DNS with TCP instead of UDP
        assert!(matches!(
            QuicListener::get_socket_address(
                &"/dns/google.com/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                    .parse()
                    .expect("valid multiaddress")
            ),
            Err(AddressError::InvalidProtocol)
        ));

        // Invalid: DNS4 with TCP instead of UDP
        assert!(matches!(
            QuicListener::get_socket_address(
                &"/dns4/example.com/tcp/5000/quic-v1".parse().expect("valid multiaddress")
            ),
            Err(AddressError::InvalidProtocol)
        ));

        // Invalid: extra protocol after quic-v1
        assert!(matches!(
            QuicListener::get_socket_address(
                &"/ip6/::1/udp/8888/quic-v1/utp".parse().expect("valid multiaddress")
            ),
            Err(AddressError::InvalidProtocol)
        ));
    }

    #[tokio::test]
    async fn no_listeners() {
        let (mut listener, _) = QuicListener::new(&Keypair::generate(), Vec::new()).unwrap();

        futures::future::poll_fn(|cx| match listener.poll_next_unpin(cx) {
            Poll::Pending => Poll::Ready(()),
            event => panic!("unexpected event: {event:?}"),
        })
        .await;
    }

    #[tokio::test]
    async fn one_listener() {
        let address: Multiaddr = "/ip6/::1/udp/0/quic-v1".parse().unwrap();
        let keypair = Keypair::generate();
        let peer = PeerId::from_public_key(&keypair.public().into());
        let (mut listener, listen_addresses) =
            QuicListener::new(&keypair, vec![address.clone()]).unwrap();
        let Some(Protocol::Udp(port)) = listen_addresses.first().unwrap().clone().iter().nth(1)
        else {
            panic!("invalid address");
        };

        let crypto_config =
            Arc::new(make_client_config(&Keypair::generate(), Some(peer)).expect("to succeed"));
        let client_config = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config).expect("to succeed"),
        ));
        let client =
            Endpoint::client(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).unwrap();
        let connection = client
            .connect_with(client_config, format!("[::1]:{port}").parse().unwrap(), "l")
            .unwrap();

        let (res1, res2) = tokio::join!(
            listener.next(),
            Box::pin(async move {
                match connection.await {
                    Ok(connection) => Ok(connection),
                    Err(error) => Err(error),
                }
            })
        );

        assert!(res1.is_some() && res2.is_ok());
    }

    #[tokio::test]
    async fn two_listeners() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let address1: Multiaddr = "/ip6/::1/udp/0/quic-v1".parse().unwrap();
        let address2: Multiaddr = "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap();
        let keypair = Keypair::generate();
        let peer = PeerId::from_public_key(&keypair.public().into());

        let (mut listener, listen_addresses) =
            QuicListener::new(&keypair, vec![address1, address2]).unwrap();

        let Some(Protocol::Udp(port1)) = listen_addresses.first().unwrap().clone().iter().nth(1)
        else {
            panic!("invalid address");
        };

        let Some(Protocol::Udp(port2)) = listen_addresses.get(1).unwrap().clone().iter().nth(1)
        else {
            panic!("invalid address");
        };

        let crypto_config1 =
            Arc::new(make_client_config(&Keypair::generate(), Some(peer)).expect("to succeed"));
        let client_config1 = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config1).expect("to succeed"),
        ));
        let client1 =
            Endpoint::client(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).unwrap();
        let connection1 = client1
            .connect_with(
                client_config1,
                format!("[::1]:{port1}").parse().unwrap(),
                "l",
            )
            .unwrap();

        let crypto_config2 =
            Arc::new(make_client_config(&Keypair::generate(), Some(peer)).expect("to succeed"));
        let client_config2 = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config2).expect("to succeed"),
        ));
        let client2 =
            Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)).unwrap();
        let connection2 = client2
            .connect_with(
                client_config2,
                format!("127.0.0.1:{port2}").parse().unwrap(),
                "l",
            )
            .unwrap();

        tokio::spawn(async move {
            match connection1.await {
                Ok(connection) => Ok(connection),
                Err(error) => Err(error),
            }
        });

        tokio::spawn(async move {
            match connection2.await {
                Ok(connection) => Ok(connection),
                Err(error) => Err(error),
            }
        });

        for _ in 0..2 {
            let _ = listener.next().await;
        }
    }

    #[tokio::test]
    async fn two_clients_dialing_same_address() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair = Keypair::generate();
        let peer = PeerId::from_public_key(&keypair.public().into());

        let (mut listener, listen_addresses) = QuicListener::new(
            &keypair,
            vec![
                "/ip6/::1/udp/0/quic-v1".parse().unwrap(),
                "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            ],
        )
        .unwrap();

        let Some(Protocol::Udp(port)) = listen_addresses.first().unwrap().clone().iter().nth(1)
        else {
            panic!("invalid address");
        };

        let crypto_config1 =
            Arc::new(make_client_config(&Keypair::generate(), Some(peer)).expect("to succeed"));
        let client_config1 = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config1).expect("to succeed"),
        ));
        let client1 =
            Endpoint::client(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).unwrap();
        let connection1 = client1
            .connect_with(
                client_config1,
                format!("[::1]:{port}").parse().unwrap(),
                "l",
            )
            .unwrap();

        let crypto_config2 =
            Arc::new(make_client_config(&Keypair::generate(), Some(peer)).expect("to succeed"));
        let client_config2 = ClientConfig::new(Arc::new(
            QuicClientConfig::try_from(crypto_config2).expect("to succeed"),
        ));
        let client2 =
            Endpoint::client(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)).unwrap();
        let connection2 = client2
            .connect_with(
                client_config2,
                format!("[::1]:{port}").parse().unwrap(),
                "l",
            )
            .unwrap();

        tokio::spawn(async move {
            match connection1.await {
                Ok(connection) => Ok(connection),
                Err(error) => Err(error),
            }
        });

        tokio::spawn(async move {
            match connection2.await {
                Ok(connection) => Ok(connection),
                Err(error) => Err(error),
            }
        });

        for _ in 0..2 {
            let _ = listener.next().await;
        }
    }
}
