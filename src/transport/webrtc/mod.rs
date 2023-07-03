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
    error::{AddressError, Error},
    peer_id::PeerId,
    transport::{Transport, TransportCommand, TransportContext},
    types::ConnectionId,
};

use futures::{future::BoxFuture, stream::FuturesUnordered};
use multiaddr::{Multiaddr, Protocol};
use str0m::{Candidate, Rtc};
use tokio::{net::UdpSocket, sync::mpsc::Receiver};

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
};

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc";

/// Convert `SocketAddr` to `Multiaddr`
fn socket_addr_to_multi_addr(address: &SocketAddr) -> Multiaddr {
    let mut multiaddr = Multiaddr::from(address.ip());
    multiaddr.push(Protocol::Udp(address.port()));
    multiaddr.push(Protocol::P2pWebRtcDirect);

    multiaddr
}

#[derive(Debug)]
pub struct WebRtcConfig {
    listen_address: Multiaddr,
}

/// WebRTC transport.
pub(crate) struct WebRtcTransport {
    /// Transport context.
    context: TransportContext,

    /// UDP socket.
    socket: UdpSocket,

    /// Str0m RTC service.
    rtc: Rtc,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Next connection ID.
    next_connection_id: ConnectionId,

    /// Pending dials.
    pending_dials: HashMap<ConnectionId, Multiaddr>,

    /// Pending connections.
    pending_connections: FuturesUnordered<BoxFuture<'static, ()>>,

    /// RX channel for receiving commands from `Litep2p`.
    rx: Receiver<TransportCommand>,
}

impl WebRtcTransport {
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
}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    type Config = WebRtcConfig;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start webrtc transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let socket = UdpSocket::bind(listen_address).await?;
        let listen_address = socket.local_addr()?;
        let rtc = {
            let mut rtc = Rtc::builder().set_ice_lite(true).build();
            let local_candidate = Candidate::host(listen_address).unwrap(); // TODO: no unwraps
            rtc.add_local_candidate(local_candidate);
            rtc
        };

        Ok(Self {
            rx,
            context,
            socket,
            rtc,
            listen_address,
            next_connection_id: ConnectionId::new(),
            pending_dials: HashMap::new(),
            pending_connections: FuturesUnordered::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        socket_addr_to_multi_addr(&self.listen_address)
    }

    /// Start transport event loop.
    async fn start(mut self) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, "start webrtc event loop");

        let mut buf = vec![0u8; 2048];

        loop {
            tokio::select! {
                event = self.socket.recv_from(&mut buf) => match event {
                    Ok((len, address)) => {
                        tracing::info!(target: LOG_TARGET, ?len, ?address, "{:?}", std::str::from_utf8(&buf));
                    }
                    Err(_) => return Err(Error::Other(String::from("failed to read from udp socket"))),
                }
            }
        }
    }
}
