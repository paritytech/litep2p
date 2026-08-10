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
use network_interface::{Addr, NetworkInterface, NetworkInterfaceConfig};
use quinn_udp::RecvMeta;
use socket2::{Domain, Socket, Type};
use tokio::net::UdpSocket;

use super::AddressPair;
use crate::{error::AddressError, transport::webrtc::socket::WebRtcSocket, Error};

const LOG_TARGET: &str = "litep2p::webrtc::listener";

/// WebRtc listener.
pub(super) struct WebRtcListener {
    /// Bound sockets paired with their local address.
    listen_sockets: Vec<(SocketAddr, Arc<WebRtcSocket>)>,

    /// Index of the socket to poll first on the next call (round-robin).
    next_listener: usize,
    /// Delay used to wake up `WebRtcListener` if all sockets errored out.
    error_delay: Option<Delay>,
}

impl WebRtcListener {
    pub(super) fn new(
        listen_addresses: Vec<Multiaddr>,
        certhash: Multihash<64>,
    ) -> crate::Result<(Self, Vec<Multiaddr>)> {
        let mut listen_multi_addresses = Vec::with_capacity(listen_addresses.len());
        let mut listen_sockets = Vec::with_capacity(listen_addresses.len());

        let handle_multiaddr = |address| -> crate::Result<(WebRtcSocket, SocketAddr)> {
            let sockaddr = Self::get_socket_address(&address)?;

            let socket = if sockaddr.is_ipv4() {
                Socket::new(Domain::IPV4, Type::DGRAM, Some(socket2::Protocol::UDP))?
            } else {
                let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(socket2::Protocol::UDP))?;
                socket.set_only_v6(true)?;
                socket
            };

            socket.bind(&sockaddr.into())?;
            socket.set_nonblocking(true)?;

            let socket = UdpSocket::from_std(socket.into())?;
            // Re-read the bound address to resolve an ephemeral port (`/udp/0`).
            let sockaddr = socket.local_addr()?;
            Ok((WebRtcSocket::new(socket)?, sockaddr))
        };

        for multiaddr in listen_addresses {
            let (socket, sockaddr) = match handle_multiaddr(multiaddr) {
                Ok(res) => res,
                Err(err) => {
                    tracing::warn!(target: LOG_TARGET, ?err, "failed to bind listen address");
                    return Err(err);
                }
            };

            listen_sockets.push((sockaddr, Arc::new(socket)));
            listen_multi_addresses.extend(Self::build_listen_addresses(sockaddr, certhash)?);
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

    /// Build the multiaddresses to advertise for a socket bound to `sockaddr`.
    ///
    /// A wildcard listen address is advertised as all matching interface addresses
    /// (link-local IPv6 excluded).
    fn build_listen_addresses(
        sockaddr: SocketAddr,
        certhash: Multihash<64>,
    ) -> crate::Result<Vec<Multiaddr>> {
        let addresses: Vec<SocketAddr> = if sockaddr.ip().is_unspecified() {
            NetworkInterface::show()
                .map_err(|error| {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?error,
                        "failed to fetch network interfaces",
                    );
                    Error::Other("failed to fetch network interfaces".to_string())
                })?
                .into_iter()
                .flat_map(|iface| {
                    iface.addr.into_iter().filter_map(|iface_address| {
                        match (iface_address, sockaddr.is_ipv4()) {
                            (Addr::V4(addr), true) =>
                                Some(SocketAddr::new(IpAddr::V4(addr.ip), sockaddr.port())),
                            (Addr::V6(addr), false) => match addr.ip.segments().first() {
                                Some(0xfe80) => None,
                                _ => Some(SocketAddr::new(IpAddr::V6(addr.ip), sockaddr.port())),
                            },
                            _ => None,
                        }
                    })
                })
                .collect()
        } else {
            vec![sockaddr]
        };

        Ok(addresses
            .into_iter()
            .map(|address| {
                Multiaddr::empty()
                    .with(Protocol::from(address.ip()))
                    .with(Protocol::Udp(address.port()))
                    .with(Protocol::WebRTCDirect)
                    .with(Protocol::Certhash(certhash))
            })
            .collect())
    }

    /// Poll the sockets for an inbound read.
    ///
    /// The filled part of `buf` (`meta.len` bytes) may contain multiple GRO-coalesced
    /// datagrams of `meta.stride` bytes each, with only the last one possibly shorter.
    pub(super) fn poll_recv_from(
        &mut self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(AddressPair, RecvMeta, Arc<WebRtcSocket>)>> {
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

            loop {
                match socket.poll_recv(cx, buf) {
                    Poll::Ready(Ok(meta)) => {
                        // The local IP of the session comes from
                        // `IP_PKTINFO`/`IPV6_PKTINFO`; fall back to the
                        // bound address on platforms not reporting it.
                        let local = SocketAddr::new(
                            meta.dst_ip.unwrap_or_else(|| local.ip()),
                            local.port(),
                        );
                        if local.ip().is_unspecified() {
                            // Wildcard socket & no `dst_ip`: the local
                            // address is unknown, drop the datagram.
                            // Can't happen on *nix.
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?local,
                                "dropping datagram without destination address",
                            );
                            continue;
                        }
                        self.next_listener = idx;
                        return Poll::Ready(Ok((
                            AddressPair {
                                local,
                                remote: meta.addr,
                            },
                            meta,
                            socket.clone(),
                        )));
                    }
                    // All `UdpSocket` errors are transient, no connection to terminate
                    Poll::Ready(Err(e)) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?local,
                            ?e,
                            "failed to receive a datagram",
                        );
                        break;
                    }
                    Poll::Pending => {
                        any_pending = true;
                        break;
                    }
                }
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
