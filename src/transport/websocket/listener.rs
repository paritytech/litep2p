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

//! WebSocket listener.

use crate::{error::AddressError, Error, PeerId};

use futures::Stream;
use multiaddr::{Multiaddr, Protocol};
use socket2::{Domain, Socket, Type};
use tokio::net::{TcpListener as TokioTcpListener, TcpStream};

use std::{
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::websocket::listener";

/// WebSocket listener listening to zero or more addresses.
pub struct WebSocketListener {
    /// Listen addresses.
    listen_addresses: Vec<Multiaddr>,

    /// Listeners.
    listeners: Vec<TokioTcpListener>,
}

impl WebSocketListener {
    /// Create new [`WebSocketListener`]
    pub fn new(addresses: Vec<Multiaddr>) -> Self {
        let (listeners, listen_addresses) = addresses
            .into_iter()
            .filter_map(|address| {
                let address = Self::get_socket_address(&address).ok()?.0;
                let socket = match address.is_ipv4() {
                    false => {
                        let socket =
                            Socket::new(Domain::IPV6, Type::STREAM, Some(socket2::Protocol::TCP))
                                .ok()?;
                        socket.set_only_v6(true).ok()?;
                        socket.bind(&address.into()).ok()?;

                        socket
                    }
                    true => {
                        let socket =
                            Socket::new(Domain::IPV4, Type::STREAM, Some(socket2::Protocol::TCP))
                                .ok()?;
                        socket.bind(&address.into()).ok()?;

                        socket
                    }
                };

                socket.listen(1024).ok()?;
                socket.set_reuse_address(true).ok()?;
                socket.set_nonblocking(true).ok()?;
                #[cfg(unix)]
                socket.set_reuse_port(true).ok()?;

                let socket: std::net::TcpListener = socket.into();
                let listener = TokioTcpListener::from_std(socket).ok()?;

                let listen_address = listener.local_addr().ok()?;
                let listen_address = Multiaddr::empty()
                    .with(Protocol::from(listen_address.ip()))
                    .with(Protocol::Tcp(listen_address.port()))
                    .with(Protocol::Ws(std::borrow::Cow::Owned("/".to_string())));

                Some((listener, listen_address))
            })
            .unzip();

        Self {
            listeners,
            listen_addresses,
        }
    }

    /// Get listen addresses.
    pub fn listen_addresses(&self) -> impl Iterator<Item = &Multiaddr> {
        self.listen_addresses.iter()
    }

    /// Extract socket address and `PeerId`, if found, from `address`.
    pub fn get_socket_address(address: &Multiaddr) -> crate::Result<(SocketAddr, Option<PeerId>)> {
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

        // verify that `/ws`/`/wss` is part of the multi address
        match iter.next() {
            Some(Protocol::Ws(_address)) => {}
            Some(Protocol::Wss(_address)) => unimplemented!("secure websocket not implemented"),
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `Ws` or `Wss`"
                );
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

impl Stream for WebSocketListener {
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
        assert!(WebSocketListener::get_socket_address(
            &"/ip6/::1/tcp/8888/ws".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(WebSocketListener::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888/ws".parse().expect("valid multiaddress")
        )
        .is_ok());
        assert!(WebSocketListener::get_socket_address(
            &"/ip6/::1/tcp/8888/ws/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(WebSocketListener::get_socket_address(
            &"/ip4/127.0.0.1/tcp/8888/ws/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_ok());
        assert!(WebSocketListener::get_socket_address(
            &"/ip6/::1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
        assert!(WebSocketListener::get_socket_address(
            &"/ip4/127.0.0.1/udp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        )
        .is_err());
    }

    #[tokio::test]
    async fn no_listeners() {
        let mut listener = WebSocketListener::new(Vec::new());

        futures::future::poll_fn(|cx| match listener.poll_next_unpin(cx) {
            Poll::Pending => Poll::Ready(()),
            event => panic!("unexpected event: {event:?}"),
        })
        .await;
    }

    #[tokio::test]
    async fn one_listener() {
        let address: Multiaddr = "/ip6/::1/tcp/0/ws".parse().unwrap();
        let mut listener = WebSocketListener::new(vec![address.clone()]);
        let Some(Protocol::Tcp(port)) =
            listener.listen_addresses().next().unwrap().clone().iter().skip(1).next()
        else {
            panic!("invalid address");
        };

        let (res1, res2) =
            tokio::join!(listener.next(), TcpStream::connect(format!("[::1]:{port}")));

        assert!(res1.unwrap().is_ok() && res2.is_ok());
    }

    #[tokio::test]
    async fn two_listeners() {
        let address1: Multiaddr = "/ip6/::1/tcp/0/ws".parse().unwrap();
        let address2: Multiaddr = "/ip4/127.0.0.1/tcp/0/ws".parse().unwrap();
        let mut listener = WebSocketListener::new(vec![address1, address2]);

        let Some(Protocol::Tcp(port1)) =
            listener.listen_addresses().next().unwrap().clone().iter().skip(1).next()
        else {
            panic!("invalid address");
        };

        let Some(Protocol::Tcp(port2)) = listener
            .listen_addresses()
            .skip(1)
            .next()
            .unwrap()
            .clone()
            .iter()
            .skip(1)
            .next()
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
}
