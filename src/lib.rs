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
    config::{LiteP2pConfiguration, TransportConfig},
    crypto::ed25519::Keypair,
    error::Error,
    peer_id::PeerId,
    transport::{tcp::TcpTransport, Transport},
    types::{ConnectionId, ProtocolId, RequestId},
};

use futures::Stream;
use multiaddr::{Multiaddr, Protocol};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc::{channel, Receiver, Sender},
};
use tokio_stream::wrappers::ReceiverStream;
use transport::TransportService;

use std::{collections::HashMap, net::SocketAddr};

pub mod config;
mod crypto;
mod error;
mod peer_id;
mod protocol;
mod transport;
mod types;

// TODO: move code from `TcpTransport` to here

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

// TODO: protocols for substream events
#[derive(Debug)]
pub enum Litep2pEvent {
    SubstreamOpened(PeerId, String, Box<dyn transport::Connection>),
    SubstreamClosed(PeerId),
    ConnectionEstablished(PeerId),
    ConnectionClosed(PeerId),
}

pub struct Litep2p {
    tranports: HashMap<&'static str, Box<dyn TransportService>>,
}

// TODO: how to support multiple transports?
// TODO:  - specify all listening endpoints (by address)
// TODO:  - map events from these endpoints to global events?
// TODO: how to support bittorrent?
// TODO: this `new()` implementation is totally incorrect
impl Litep2p {
    /// Create new [`Litep2p`] object.
    pub async fn new(
        config: LiteP2pConfiguration,
    ) -> crate::Result<(Litep2p, Box<dyn Stream<Item = Litep2pEvent> + Unpin>)> {
        assert!(config.listen_addresses().count() == 1);
        let keypair = Keypair::generate();

        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);
        let handle = TcpTransport::start(
            &keypair,
            TransportConfig::new(
                config.listen_addresses().next().unwrap().clone(),
                vec!["/ipfs/ping/1.0.0".to_owned()],
                vec![],
                vec![],
                40_000,
            ),
            tx,
        )
        .await?;

        Ok((
            Self {
                tranports: HashMap::from([("tcp", handle)]),
            },
            Box::new(ReceiverStream::new(rx)),
        ))
    }

    /// Open connection to remote peer at `address`.
    ///
    /// Connection is opened and negotiated in the background and the result is
    /// indicated to the caller through [`Litep2pEvent::ConnectionEstablished`]/[`Litep2pEvent::DialFailure`]
    pub async fn open_connection(&mut self, address: Multiaddr) {
        tracing::debug!(
            target: LOG_TARGET,
            ?address,
            "establish outbound connection"
        );

        self.tranports
            .get_mut("tcp")
            .unwrap()
            .open_connection(address)
            .await;
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
