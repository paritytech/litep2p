// Copyright 2017 Parity Technologies (UK) Ltd.
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

//! TCP listener.

use crate::{error::AddressError, Error, PeerId};

use futures::Stream;
use multiaddr::{Multiaddr, Protocol};
use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use socket2::{Domain, Socket, Type};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};

use std::{
    io,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::tcp::listener";

/// Address type.
#[derive(Debug)]
pub(super) enum AddressType {
    /// Socket address.
    Socket(SocketAddr),

    /// DNS address.
    Dns(String, u16),
}

/// TCP listener listening to zero or more addresses.
pub struct TcpListener {
    /// Listeners.
    listeners: Vec<TokioTcpListener>,
}

/// Local addresses to use for outbound connections.
#[derive(Clone)]
pub enum DialAddresses {
    /// Reuse port from listen addresses.
    Reuse {
        listen_addresses: Arc<Vec<SocketAddr>>,
    },
    /// Do not reuse port.
    NoReuse,
}

impl Default for DialAddresses {
    fn default() -> Self {
        DialAddresses::NoReuse
    }
}

impl DialAddresses {
    /// Get local dial address for an outbound connection.
    pub(super) fn local_dial_address(
        &self,
        remote_address: &IpAddr,
    ) -> Result<Option<SocketAddr>, ()> {
        match self {
            DialAddresses::Reuse { listen_addresses } => {
                for address in listen_addresses.iter() {
                    if remote_address.is_ipv4() == address.is_ipv4()
                        && remote_address.is_loopback() == address.ip().is_loopback()
                    {
                        if remote_address.is_ipv4() {
                            return Ok(Some(SocketAddr::new(
                                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                                address.port(),
                            )));
                        } else {
                            return Ok(Some(SocketAddr::new(
                                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                                address.port(),
                            )));
                        }
                    }
                }

                Err(())
            }
            DialAddresses::NoReuse => Ok(None),
        }
    }
}

impl TcpListener {
    /// Create new [`TcpListener`]
    pub fn new(
        addresses: Vec<Multiaddr>,
        reuse_port: bool,
    ) -> (Self, Vec<Multiaddr>, DialAddresses) {
        let (listeners, listen_addresses): (_, Vec<Vec<_>>) = addresses
            .into_iter()
            .filter_map(|address| {
                let (socket, address) = match Self::get_socket_address(&address).ok()?.0 {
                    AddressType::Dns(_, _) => return None,
                    AddressType::Socket(address) => match address.is_ipv4() {
                        false => {
                            let socket = Socket::new(
                                Domain::IPV6,
                                Type::STREAM,
                                Some(socket2::Protocol::TCP),
                            )
                            .ok()?;
                            socket.set_only_v6(true).ok()?;
                            (socket, address)
                        }
                        true => (
                            Socket::new(Domain::IPV4, Type::STREAM, Some(socket2::Protocol::TCP))
                                .ok()?,
                            address,
                        ),
                    },
                };

                socket.set_nonblocking(true).ok()?;
                socket.set_reuse_address(true).ok()?;
                #[cfg(unix)]
                if reuse_port {
                    socket.set_reuse_port(true).ok()?;
                }
                socket.bind(&address.into()).ok()?;
                socket.listen(1024).ok()?;

                let socket: std::net::TcpListener = socket.into();
                let listener = TokioTcpListener::from_std(socket).ok()?;
                let local_address = listener.local_addr().ok()?;

                let listen_addresses = match address.ip().is_unspecified() {
                    true => match NetworkInterface::show() {
                        Ok(ifaces) => ifaces
                            .into_iter()
                            .flat_map(|record| {
                                record.addr.into_iter().filter_map(|iface_address| {
                                    match (iface_address, address.is_ipv4()) {
                                        (Addr::V4(inner), true) => Some(SocketAddr::new(
                                            IpAddr::V4(inner.ip),
                                            local_address.port(),
                                        )),
                                        (Addr::V6(inner), false) =>
                                            match inner.ip.segments().get(0) {
                                                Some(0xfe80) => None,
                                                _ => Some(SocketAddr::new(
                                                    IpAddr::V6(inner.ip),
                                                    local_address.port(),
                                                )),
                                            },
                                        _ => None,
                                    }
                                })
                            })
                            .collect(),
                        Err(error) => {
                            tracing::warn!(
                                target: LOG_TARGET,
                                ?error,
                                "failed to fetch network interfaces",
                            );

                            return None;
                        }
                    },
                    false => vec![local_address],
                };

                Some((listener, listen_addresses))
            })
            .unzip();

        let listen_addresses = listen_addresses.into_iter().flatten().collect::<Vec<_>>();
        let listen_multi_addresses = listen_addresses
            .iter()
            .cloned()
            .map(|address| {
                Multiaddr::empty()
                    .with(Protocol::from(address.ip()))
                    .with(Protocol::Tcp(address.port()))
            })
            .collect();
        let dial_addresses = if reuse_port {
            DialAddresses::Reuse {
                listen_addresses: Arc::new(listen_addresses),
            }
        } else {
            DialAddresses::NoReuse
        };

        (Self { listeners }, listen_multi_addresses, dial_addresses)
    }

    /// Extract socket address and `PeerId`, if found, from `address`.
    pub(super) fn get_socket_address(
        address: &Multiaddr,
    ) -> crate::Result<(AddressType, Option<PeerId>)> {
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
}

impl Stream for TcpListener {
    type Item = io::Result<(TcpStream, SocketAddr)>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.listeners.is_empty() {
            return Poll::Pending;
        }

        // TODO: make this more fair
        for listener in self.listeners.iter_mut() {
            match listener.poll_accept(cx) {
                Poll::Pending => {}
                Poll::Ready(Err(error)) => return Poll::Ready(Some(Err(error))),
                Poll::Ready(Ok((stream, address))) =>
                    return Poll::Ready(Some(Ok((stream, address)))),
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn parse_multiaddresses() {
        assert!(TcpListener::get_socket_address(
            &"/ip6/::1/tcp/8888".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpListener::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpListener::get_socket_address(
            &"/ip6/::1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpListener::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(TcpListener::get_socket_address(
            &"/ip6/::1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
        assert!(TcpListener::get_socket_address(
            &"/ip4/127.0.0.1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
    }

    #[tokio::test]
    async fn no_listeners() {
        let (mut listener, _, _) = TcpListener::new(Vec::new(), true);

        futures::future::poll_fn(|cx| match listener.poll_next_unpin(cx) {
            Poll::Pending => Poll::Ready(()),
            event => panic!("unexpected event: {event:?}"),
        })
        .await;
    }

    #[tokio::test]
    async fn one_listener() {
        let address: Multiaddr = "/ip6/::1/tcp/0".parse().unwrap();
        let (mut listener, listen_addresses, _) = TcpListener::new(vec![address.clone()], true);
        let Some(Protocol::Tcp(port)) =
            listen_addresses.iter().next().unwrap().clone().iter().skip(1).next()
        else {
            panic!("invalid address");
        };

        let (res1, res2) =
            tokio::join!(listener.next(), TcpStream::connect(format!("[::1]:{port}")));

        assert!(res1.unwrap().is_ok() && res2.is_ok());
    }

    #[tokio::test]
    async fn two_listeners() {
        let address1: Multiaddr = "/ip6/::1/tcp/0".parse().unwrap();
        let address2: Multiaddr = "/ip4/127.0.0.1/tcp/0".parse().unwrap();
        let (mut listener, listen_addresses, _) = TcpListener::new(vec![address1, address2], true);
        let Some(Protocol::Tcp(port1)) =
            listen_addresses.iter().next().unwrap().clone().iter().skip(1).next()
        else {
            panic!("invalid address");
        };

        let Some(Protocol::Tcp(port2)) =
            listen_addresses.iter().skip(1).next().unwrap().clone().iter().skip(1).next()
        else {
            panic!("invalid address");
        };

        tokio::spawn(async move { while let Some(_) = listener.next().await {} });

        let (res1, res2) = tokio::join!(
            TcpStream::connect(format!("[::1]:{port1}")),
            TcpStream::connect(format!("127.0.0.1:{port2}"))
        );

        assert!(res1.is_ok() && res2.is_ok());
    }

    #[tokio::test]
    async fn local_dial_address() {
        let dial_addresses = DialAddresses::Reuse {
            listen_addresses: Arc::new(vec![
                "[2001:7d0:84aa:3900:2a5d:9e85::]:8888".parse().unwrap(),
                "92.168.127.1:9999".parse().unwrap(),
            ]),
        };

        assert_eq!(
            dial_addresses.local_dial_address(&IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1))),
            Ok(Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                9999
            ))),
        );

        assert_eq!(
            dial_addresses.local_dial_address(&IpAddr::V6(Ipv6Addr::new(0, 1, 2, 3, 4, 5, 6, 7))),
            Ok(Some(SocketAddr::new(
                IpAddr::V6(Ipv6Addr::UNSPECIFIED),
                8888
            ))),
        );
    }

    #[tokio::test]
    async fn show_all_addresses() {
        let address1: Multiaddr = "/ip6/::/tcp/0".parse().unwrap();
        let address2: Multiaddr = "/ip4/0.0.0.0/tcp/0".parse().unwrap();
        let (_, listen_addresses, _) = TcpListener::new(vec![address1, address2], true);

        println!("{listen_addresses:#?}");
    }
}
