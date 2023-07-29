// Copyright 2018 Parity Technologies (UK) Ltd.
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

use crate::{error::Error, transport::TransportContext};

use multiaddr::Multiaddr;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    time::Duration,
};

/// Logging target for the file.
const LOG_TARGET: &str = "mdns";

/// IPv4 multicast address.
const IPV4_MULTICAST_ADDRESS: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);

/// IPV4 multicast port.
const IPV4_MULTICAST_PORT: u16 = 5353;

/// mDNS configuration.
#[derive(Debug)]
pub struct Config {
    /// How often the network should be queried for new peers.
    query_interval: Duration,
}

pub struct Mdns {
    /// UDP socket for multicast requests/responses.
    socket: UdpSocket,

    /// mDNS configuration.
    config: Config,

    /// Transport context.
    context: TransportContext,

    /// Buffer for incoming messages.
    receive_buffer: Vec<u8>,

    /// Listen addresses.
    listen_addresses: Vec<Multiaddr>,
}

impl Mdns {
    /// Create new [`Mdns`].
    pub fn new(
        config: Config,
        context: TransportContext,
        listen_addresses: Vec<Multiaddr>,
    ) -> crate::Result<Self> {
        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        #[cfg(unix)]
        socket.set_reuse_port(true)?;
        socket.bind(
            &SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), IPV4_MULTICAST_PORT).into(),
        )?;
        socket.set_multicast_loop_v4(true)?;
        socket.set_multicast_ttl_v4(255)?;
        socket.join_multicast_v4(&IPV4_MULTICAST_ADDRESS, &Ipv4Addr::UNSPECIFIED)?;

        Ok(Self {
            config,
            context,
            listen_addresses,
            receive_buffer: Vec::with_capacity(4096),
            socket: UdpSocket::from_std(std::net::UdpSocket::from(socket))?,
        })
    }

    /// Event loop for [`Mdns`].
    pub(crate) async fn start(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                result = self.socket.recv_from(&mut self.receive_buffer) => match result {
                    Ok((bytes_read, address)) => {
                        tracing::trace!(target: LOG_TARGET, ?bytes_read, ?address, "read data from socket");
                    }
                    Err(error) => {
                        tracing::error!(target: LOG_TARGET, ?error, "failed to read from socket");
                        return Err(Error::from(error));
                    }
                },
                _ = tokio::time::sleep(self.config.query_interval) => {
                    todo!();
                }
            }
        }
    }
}
