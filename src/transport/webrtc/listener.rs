use std::{
    future::Future,
    io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures_timer::Delay;
use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use socket2::{Domain, Socket, Type};
use tokio::{io::ReadBuf, net::UdpSocket};

use super::AddressPair;
use crate::{error::AddressError, Error};

const LOG_TARGET: &str = "litep2p::webrtc::listener";

/// WebRtc listener.
pub(super) struct WebRtcListener {
    /// Bound sockets paired with their local address.
    listen_sockets: Vec<(SocketAddr, Arc<UdpSocket>)>,

    /// Index of the socket to poll first on the next call (round-robin).
    next_listener: usize,
    /// Delay used to wake up `WebRtcListener` if all sockets errored out.
    error_delay: Option<Delay>,
}

impl WebRtcListener {
    pub(super) fn new(
        multiaddr_listen_addresses: Vec<Multiaddr>,
        certificate: Multihash<64>,
    ) -> crate::Result<(Self, Vec<Multiaddr>)> {
        let mut listen_multi_addresses = Vec::with_capacity(multiaddr_listen_addresses.len());
        let mut listen_sockets = Vec::with_capacity(multiaddr_listen_addresses.len());

        let handle_multiaddr = |listen_socket| -> crate::Result<(UdpSocket, SocketAddr)> {
            let listen_socket = Self::get_socket_address(&listen_socket)?;
            if listen_socket.ip().is_unspecified() {
                return Err(Error::Other(
                    "WebRTC cannot listen on an unspecified address".to_string(),
                ));
            }

            let socket = if listen_socket.is_ipv4() {
                Socket::new(Domain::IPV4, Type::DGRAM, Some(socket2::Protocol::UDP))?
            } else {
                let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(socket2::Protocol::UDP))?;
                socket.set_only_v6(true)?;
                socket
            };

            socket.bind(&listen_socket.into())?;
            socket.set_nonblocking(true)?;

            let socket = UdpSocket::from_std(socket.into())?;
            let listen_socket = socket.local_addr()?;
            Ok((socket, listen_socket))
        };

        for listen_socket in multiaddr_listen_addresses {
            let (socket, listen_socket) = match handle_multiaddr(listen_socket) {
                Ok(res) => res,
                Err(err) => {
                    tracing::warn!(target: LOG_TARGET, ?err, "failed to bind listen address, skipping");
                    continue;
                }
            };

            listen_sockets.push((listen_socket, Arc::new(socket)));
            listen_multi_addresses.push(
                Multiaddr::empty()
                    .with(Protocol::from(listen_socket.ip()))
                    .with(Protocol::Udp(listen_socket.port()))
                    .with(Protocol::WebRTCDirect)
                    .with(Protocol::Certhash(certificate)),
            );
        }

        if listen_sockets.is_empty() {
            return Err(Error::Other(
                "WebRtcListener requires at least one valid listen address".to_string(),
            ));
        }

        Ok((
            Self {
                listen_sockets,
                next_listener: 0,
                error_delay: None,
            },
            listen_multi_addresses,
        ))
    }

    pub(super) fn socket(&self, local: &SocketAddr) -> Option<Arc<UdpSocket>> {
        self.listen_sockets
            .iter()
            .find(|(addr, _)| local == addr)
            .map(|(_, socket)| socket.clone())
    }

    pub(super) fn poll_recv_from(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<AddressPair>> {
        let n_listener = self.listen_sockets.len();
        debug_assert!(n_listener > 0);

        if let Some(delay) = self.error_delay.as_mut() {
            if Pin::new(delay).poll(cx).is_ready() {
                self.error_delay = None;
            } else {
                // timer registers cx's waker
                return Poll::Pending;
            }
        }

        let mut idx = self.next_listener;
        let mut any_pending = false;

        loop {
            let (local, socket) = &self.listen_sockets[idx];
            idx = (idx + 1) % n_listener;

            match socket.poll_recv_from(cx, buf) {
                Poll::Ready(Ok(source)) => {
                    self.next_listener = idx;
                    return Poll::Ready(Ok(AddressPair {
                        local: *local,
                        source,
                    }));
                }
                // All UdpSocket errors are transient and noone
                // of them implies a complete shutdown of the socket.
                // Log the error but do not tear down the WebRtc instance.
                Poll::Ready(Err(e)) => tracing::debug!(
                    target: LOG_TARGET,
                    ?local,
                    ?e,
                    "failed to receive a datagram",
                ),
                Poll::Pending => any_pending = true,
            }

            // Each socket that returned Pending registered its waker,
            // Err sockets did not but will re-register on the next poll.
            if idx == self.next_listener {
                if !any_pending {
                    let mut delay = Delay::new(Duration::from_millis(10));
                    let _ = Pin::new(&mut delay).poll(cx);
                    self.error_delay = Some(delay);
                }
                return Poll::Pending;
            }
        }
    }

    /// Extract socket address, if found, from `address`.
    ///
    /// Also verifies that the specified multiaddress is a webrtc address.
    fn get_socket_address(address: &Multiaddr) -> crate::Result<SocketAddr> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse multi address");

        let mut iter = address.iter();
        let socket_address = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V6(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Upd`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            Some(Protocol::Ip4(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V4(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Udp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            protocol => {
                tracing::error!(target: LOG_TARGET, ?protocol, "invalid transport protocol");
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        match (iter.next(), iter.next()) {
            (Some(Protocol::WebRTCDirect), None) => {}
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `WebRTCDirect` with no trailing protocols"
                );
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        }

        Ok(socket_address)
    }
}
