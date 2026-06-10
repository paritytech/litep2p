use std::{
    collections::HashSet,
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
        // De-duplicate bound addresses to avoid binding the same socket twice.
        let mut bound = HashSet::new();

        for listen_address in multiaddr_listen_addresses {
            let listen_socket = Self::get_socket_address(&listen_address)?;
            let is_unspecified = listen_socket.ip().is_unspecified();
            let bind_addresses = Self::expand_listen_address(listen_socket)?;

            for bind_address in bind_addresses {
                if !bound.insert(bind_address) {
                    continue;
                }

                let (socket, local_socket) = match Self::bind_socket(bind_address) {
                    Ok(res) => res,
                    // Expanded interface addresses may fail to bind; only explicit ones are fatal.
                    Err(err) if is_unspecified => {
                        tracing::warn!(
                            target: LOG_TARGET,
                            ?bind_address,
                            ?err,
                            "failed to bind interface address, skipping",
                        );
                        bound.remove(&bind_address);
                        continue;
                    }
                    Err(err) => {
                        tracing::warn!(target: LOG_TARGET, ?err, "failed to bind listen address");
                        return Err(err);
                    }
                };

                listen_sockets.push((local_socket, Arc::new(socket)));
                listen_multi_addresses.push(
                    Multiaddr::empty()
                        .with(Protocol::from(local_socket.ip()))
                        .with(Protocol::Udp(local_socket.port()))
                        .with(Protocol::WebRTCDirect)
                        .with(Protocol::Certhash(certificate)),
                );
            }
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

    /// Bind a non-blocking UDP socket to `listen_socket` and return it together with the
    /// concrete local address it was bound to.
    fn bind_socket(listen_socket: SocketAddr) -> crate::Result<(UdpSocket, SocketAddr)> {
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
        let local_socket = socket.local_addr()?;
        Ok((socket, local_socket))
    }

    /// Expand a listen address into the concrete socket addresses to bind to.
    ///
    /// Concrete addresses pass through unchanged. Unspecified addresses (`0.0.0.0` / `[::]`)
    /// are expanded into one address per matching local interface, since a socket bound to
    /// `0.0.0.0` reports `0.0.0.0` as its local IP, which `str0m` rejects as an ICE candidate.
    fn expand_listen_address(listen_socket: SocketAddr) -> crate::Result<Vec<SocketAddr>> {
        if !listen_socket.ip().is_unspecified() {
            return Ok(vec![listen_socket]);
        }

        let port = listen_socket.port();
        let want_ipv4 = listen_socket.is_ipv4();

        let interfaces = NetworkInterface::show()
            .map_err(|error| Error::Other(format!("failed to fetch network interfaces: {error}")))?;

        let addresses: Vec<SocketAddr> = interfaces
            .into_iter()
            .flat_map(|iface| iface.addr.into_iter())
            .filter_map(|addr| match (addr, want_ipv4) {
                (Addr::V4(inner), true) => Some(SocketAddr::new(IpAddr::V4(inner.ip), port)),
                // Skip IPv6 link-local (`fe80::/10`): unusable as an ICE candidate.
                (Addr::V6(inner), false) if inner.ip.segments()[0] != 0xfe80 =>
                    Some(SocketAddr::new(IpAddr::V6(inner.ip), port)),
                _ => None,
            })
            .collect();

        if addresses.is_empty() {
            return Err(Error::Other(format!(
                "WebRTC listener found no concrete {} interface address to bind to",
                if want_ipv4 { "IPv4" } else { "IPv6" },
            )));
        }

        Ok(addresses)
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

#[cfg(test)]
mod tests {
    use super::*;
    use multihash_codetable::MultihashDigest;

    fn test_certhash() -> Multihash<64> {
        multihash_codetable::Code::Sha2_256.digest(b"litep2p-webrtc-test")
    }

    #[tokio::test]
    async fn concrete_listen_address_is_bound() {
        let (listener, addresses) = WebRtcListener::new(
            vec!["/ip4/127.0.0.1/udp/0/webrtc-direct".parse().unwrap()],
            test_certhash(),
        )
        .expect("binding a concrete loopback address should succeed");

        assert_eq!(listener.listen_sockets.len(), 1);
        assert_eq!(addresses.len(), 1);
        let bound = listener.listen_sockets[0].0;
        assert_eq!(bound.ip(), IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        assert_ne!(bound.port(), 0);
    }

    #[tokio::test]
    async fn unspecified_ipv4_listen_address_is_expanded() {
        let (listener, addresses) = WebRtcListener::new(
            vec!["/ip4/0.0.0.0/udp/0/webrtc-direct".parse().unwrap()],
            test_certhash(),
        )
        .expect("unspecified address should expand to at least loopback");

        assert!(!listener.listen_sockets.is_empty());
        assert_eq!(listener.listen_sockets.len(), addresses.len());

        // Every bound socket and advertised address must carry a concrete IPv4 address.
        for (bound, _) in &listener.listen_sockets {
            assert!(bound.is_ipv4());
            assert!(!bound.ip().is_unspecified());
        }
        for address in &addresses {
            let mut iter = address.iter();
            match iter.next() {
                Some(Protocol::Ip4(ip)) => assert!(!ip.is_unspecified()),
                protocol => panic!("expected concrete ip4 address, got {protocol:?}"),
            }
            assert!(matches!(iter.next(), Some(Protocol::Udp(_))));
            assert!(matches!(iter.next(), Some(Protocol::WebRTCDirect)));
            assert!(matches!(iter.next(), Some(Protocol::Certhash(_))));
        }

        // Loopback is always present in the expansion.
        assert!(listener
            .listen_sockets
            .iter()
            .any(|(addr, _)| addr.ip() == IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)));
    }

    #[tokio::test]
    async fn duplicate_addresses_are_bound_once() {
        // Two identical addresses must collapse to a single bind (port 0 keeps them
        // colliding on the same pre-bind key `127.0.0.1:0`).
        let (listener, addresses) = WebRtcListener::new(
            vec![
                "/ip4/127.0.0.1/udp/0/webrtc-direct".parse().unwrap(),
                "/ip4/127.0.0.1/udp/0/webrtc-direct".parse().unwrap(),
            ],
            test_certhash(),
        )
        .expect("binding loopback should succeed");

        assert_eq!(listener.listen_sockets.len(), 1);
        assert_eq!(addresses.len(), 1);
    }
}
