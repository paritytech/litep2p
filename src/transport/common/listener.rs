// Copyright 2024 litep2p developers
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

//! Shared socket listener between TCP and WebSocket.

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
const LOG_TARGET: &str = "litep2p::transport::listener";

/// Address type.
#[derive(Debug)]
pub(super) enum AddressType {
    /// Socket address.
    Socket(SocketAddr),

    /// DNS address.
    Dns(String, u16),
}

/// Local addresses to use for outbound connections.
#[derive(Clone, Default)]
pub enum DialAddresses {
    /// Reuse port from listen addresses.
    Reuse {
        listen_addresses: Arc<Vec<SocketAddr>>,
    },
    /// Do not reuse port.
    #[default]
    NoReuse,
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

/// Socket listening to zero or more addresses.
pub struct SocketListener {
    /// Listeners.
    listeners: Vec<TokioTcpListener>,
    /// The index in the listeners from which the polling is resumed.
    poll_index: usize,
}

/// The type of the socket listener.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SocketListenerType {
    Tcp,
    WebSocket,
}

impl SocketListener {
    /// Create new [`SocketListener`]
    pub fn new(
        addresses: Vec<Multiaddr>,
        reuse_port: bool,
        ty: SocketListenerType,
    ) -> (Self, Vec<Multiaddr>, DialAddresses) {
        let (listeners, listen_addresses): (_, Vec<Vec<_>>) = addresses
            .into_iter()
            .filter_map(|address| {
                let address = match Self::get_socket_address(&address, ty).ok()?.0 {
                    AddressType::Dns(address, port) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?address,
                            ?port,
                            "dns not supported as bind address"
                        );

                        return None;
                    }
                    AddressType::Socket(address) => address,
                };

                let socket = if address.is_ipv4() {
                    Socket::new(Domain::IPV4, Type::STREAM, Some(socket2::Protocol::TCP)).ok()?
                } else {
                    let socket =
                        Socket::new(Domain::IPV6, Type::STREAM, Some(socket2::Protocol::TCP))
                            .ok()?;
                    socket.set_only_v6(true).ok()?;
                    socket
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

                let listen_addresses = if address.ip().is_unspecified() {
                    match NetworkInterface::show() {
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
                                            match inner.ip.segments().first() {
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
                    }
                } else {
                    vec![local_address]
                };

                Some((listener, listen_addresses))
            })
            .unzip();

        let listen_addresses = listen_addresses.into_iter().flatten().collect::<Vec<_>>();
        let listen_multi_addresses = listen_addresses
            .iter()
            .cloned()
            .map(|address| {
                let multi_addr = Multiaddr::empty()
                    .with(Protocol::from(address.ip()))
                    .with(Protocol::Tcp(address.port()));

                match ty {
                    SocketListenerType::Tcp => multi_addr,
                    SocketListenerType::WebSocket =>
                        multi_addr.with(Protocol::Ws(std::borrow::Cow::Owned("/".to_string()))),
                }
            })
            .collect();
        let dial_addresses = if reuse_port {
            DialAddresses::Reuse {
                listen_addresses: Arc::new(listen_addresses),
            }
        } else {
            DialAddresses::NoReuse
        };

        (
            Self {
                listeners,
                poll_index: 0,
            },
            listen_multi_addresses,
            dial_addresses,
        )
    }

    /// Extract socket address and `PeerId`, if found, from `address`.
    pub(super) fn get_socket_address(
        address: &Multiaddr,
        ty: SocketListenerType,
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

        match ty {
            SocketListenerType::Tcp => (),
            SocketListenerType::WebSocket => {
                // verify that `/ws`/`/wss` is part of the multi address
                match iter.next() {
                    Some(Protocol::Ws(_address)) => {}
                    Some(Protocol::Wss(_address)) => {}
                    protocol => {
                        tracing::error!(
                            target: LOG_TARGET,
                            ?protocol,
                            "invalid protocol, expected `Ws` or `Wss`"
                        );
                        return Err(Error::AddressError(AddressError::InvalidProtocol));
                    }
                };
            }
        }

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

impl Stream for SocketListener {
    type Item = io::Result<(TcpStream, SocketAddr)>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.listeners.is_empty() {
            return Poll::Pending;
        }

        let len = self.listeners.len();
        for index in 0..len {
            let current = (self.poll_index + index) % len;
            let listener = &mut self.listeners[current];

            match listener.poll_accept(cx) {
                Poll::Pending => {}
                Poll::Ready(Err(error)) => {
                    self.poll_index = (self.poll_index + 1) % len;
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(Ok((stream, address))) => {
                    self.poll_index = (self.poll_index + 1) % len;
                    return Poll::Ready(Some(Ok((stream, address))));
                }
            }
        }

        Poll::Pending
    }
}
