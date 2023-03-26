// Copyright 2020 Parity Technologies (UK) Ltd.
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

#![allow(unused)]

use crate::{
    config::LiteP2pConfiguration,
    error::Error,
    types::{ConnectionId, PeerId, ProtocolId, RequestId},
};

use multiaddr::{Multiaddr, Protocol};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

use std::{collections::HashMap, net::SocketAddr};

mod config;
mod crypto;
mod error;
mod peer_id;
mod protocol;
mod transport;
mod types;

/// Public result type used by the crate.
pub type Result<T> = std::result::Result<T, error::Error>;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p";

/// Default channel size.
const DEFAULT_CHANNEL_SIZE: usize = 64usize;

struct ConnectionContext {
    _connection: ConnectionId,
}

struct RequestContext {
    _peer: PeerId,
}

pub struct Litep2p {
    _connections: HashMap<ConnectionId, ConnectionContext>,
    _pending_requests: HashMap<RequestId, RequestContext>,
}

// TODO: how to support multiple transports?
// TODO:  - specify all listening endpoints (by address)
// TODO:  - map events from these endpoints to global events?
// TODO: how to support bittorrent?
impl Litep2p {
    /// Create new [`Litep2p`] object.
    pub fn new(_config: LiteP2pConfiguration) -> Self {
        Self {
            _connections: HashMap::new(),
            _pending_requests: HashMap::new(),
        }
    }

    /// Open connection to remote peer at `address`.
    // TODO: this can't block, make it async?
    // TODO: how to implement rate-limiting for a connection
    pub async fn open_connection(&mut self, address: Multiaddr) -> crate::Result<PeerId> {
        let context = match address.iter().skip(1).next() {
            Some(Protocol::Tcp(_)) => {
                todo!();
            }
            protocol => {
                tracing::error!(target: LOG_TARGET, ?protocol, "unsupported protocol");
                return Err(Error::AddressError(error::AddressError::InvalidProtocol));
            }
        };

        // TODO: verify that transport layer protocol is either ip6 or ip4
        // TODO: verify that application-layer protocol is one of supported
        // TODO: how to verify that there is no connection open to peer yet?
        // TODO: create new connection for selected transport (TcpStream)?
        // TODO: make it noise-encrypted
        // TODO: use yamux
        todo!();
    }

    /// Close connection to remote `peer`.
    pub fn close_connection(&mut self, _peer: PeerId) -> crate::Result<()> {
        todo!();
    }

    /// Open notification substream with `peer` for `protocol`.
    // TODO: return (handle, sink) pair that allows sending and receiving notifications from the substream?
    // TODO: how to implement rate-limiting for substream?
    pub fn open_substream(&mut self, _peer: PeerId, _protocol: ProtocolId) -> crate::Result<()> {
        todo!();
    }

    /// Send request to `peer` over `protocol`
    // TODO: timeouts
    // TODO: rate-limiting?
    pub fn send_request(
        &mut self,
        _peer: PeerId,
        _protocol: ProtocolId,
        _request: Vec<u8>,
    ) -> crate::Result<()> {
        // TODO: open substream and send request
        todo!();
    }

    /// Send response to `request`.
    pub fn send_response(&mut self, _request: RequestId, _response: Vec<u8>) -> crate::Result<()> {
        // TODO: verify that substream for `request` exists
        // TODO: get sink for that substream and send request
        // TODO: close substream
        todo!();
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn two_transport_supported() {
        assert_eq!(1 + 1, 2);
    }
}
