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
}
