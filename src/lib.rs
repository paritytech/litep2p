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
    protocol::libp2p::{IpfsPing, Libp2pProtocol, Libp2pProtocolEvent},
    transport::{tcp::TcpTransport, Connection, Transport, TransportEvent},
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

#[derive(Debug)]
pub enum Litep2pEvent {
    /// Connection established.
    ConnectionEstablished(PeerId),

    /// Connection closed.
    ConnectionClosed(PeerId),

    /// Dial failure for outbound connection.
    DialFailure(Multiaddr),
}

/// [`Litep2p`] object.
pub struct Litep2p {
    /// Enable transports.
    tranports: HashMap<&'static str, Box<dyn TransportService>>,

    /// Receiver for events received from enabled transports.
    transport_rx: Receiver<TransportEvent>,

    /// RX channel for receiving events from libp2p standard protocols.
    libp2p_rx: Receiver<Libp2pProtocolEvent>,

    /// TX channels for communicating with libp2p standard protocols.
    libp2p_tx: HashMap<String, Sender<TransportEvent>>,
}

impl Litep2p {
    /// Create new [`Litep2p`].
    pub async fn new(config: LiteP2pConfiguration) -> crate::Result<Litep2p> {
        assert!(config.listen_addresses().count() == 1);
        let keypair = Keypair::generate();

        let (transport_tx, transport_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let (libp2p_tx, libp2p_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let handle = TcpTransport::start(
            &keypair,
            TransportConfig::new(
                config.listen_addresses().next().unwrap().clone(),
                vec!["/ipfs/ping/1.0.0".to_owned()],
                vec![],
                vec![],
                40_000,
            ),
            transport_tx,
        )
        .await?;

        // initialize libp2p standard protocols
        // TODO: this will be ugly
        let (ping_tx, ping_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let ping = IpfsPing::start(ping_rx);

        Ok(Self {
            tranports: HashMap::from([("tcp", handle)]),
            transport_rx,
            libp2p_rx,
            libp2p_tx: HashMap::from([(String::from("/ipfs/ping/1.0.0"), ping_tx)]),
        })
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
    pub async fn close_connection(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "close connection");

        self.tranports
            .get_mut("tcp")
            .unwrap()
            .close_connection(peer)
            .await;
    }

    /// Handle open substreamed.
    async fn on_substream_opened(
        &mut self,
        protocol: &String,
        peer: PeerId,
        substream: Box<dyn Connection>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?peer, "substream opened");

        if let Some(tx) = self.libp2p_tx.get_mut(protocol) {
            return tx
                .send(TransportEvent::SubstreamOpened(
                    protocol.clone(),
                    peer,
                    substream,
                ))
                .await
                .map_err(From::from);
        }

        // TODO: match on `protocol`
        // TODO: if `protocol` is a libp2p protocol, handle internally
        // TODO: if `protocol` is a user-installed notification protocol,
        //       dispatch the substream to protocol handler
        // TODO: if `protocol` is a user-isntalled request-response protocol,
        //       read request from substream and send it request handler
        Ok(())
    }

    /// Handle closed substream.
    async fn on_substream_closed(&mut self, protocol: &String, peer: PeerId) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?peer, "substream closed");

        if let Some(tx) = self.libp2p_tx.get_mut(protocol) {
            return tx
                .send(TransportEvent::SubstreamClosed(protocol.clone(), peer))
                .await
                .map_err(From::from);
        }

        // TODO: if `protocol` is a user-installed notification protocol,
        //       send the closing information to protocol handler
        // TODO: if `protocol` is a user-isntalled request-response protocol,
        //       send the information (what exactly??) to request handler
        //         - if waiting for response, request is refused
        //         - if waiting request to be sent, request was cancelled

        Ok(())
    }

    /// Event loop for [`Litep2p`].
    pub async fn next_event(&mut self) -> crate::Result<Litep2pEvent> {
        loop {
            tokio::select! {
                event = self.transport_rx.recv() => match event {
                    Some(TransportEvent::SubstreamOpened(protocol, peer, substream)) => {
                        if let Err(err) = self.on_substream_opened(&protocol, peer, substream).await {
                            tracing::error!(
                                target: LOG_TARGET,
                                ?protocol,
                                ?peer,
                                "failed to notify protocol that a substream was opened",
                            );
                            return Err(err);
                        }
                    }
                    Some(TransportEvent::SubstreamClosed(protocol, peer)) => {
                        if let Err(err) = self.on_substream_closed(&protocol, peer).await {
                            tracing::error!(
                                target: LOG_TARGET,
                                ?protocol,
                                ?peer,
                                "failed to notify protocol that a substream was closed",
                            );
                            return Err(err);
                        }
                    }
                    Some(TransportEvent::ConnectionEstablished(peer)) => {
                        return Ok(Litep2pEvent::ConnectionEstablished(peer));
                    }
                    Some(TransportEvent::ConnectionClosed(peer)) => {
                        return Ok(Litep2pEvent::ConnectionClosed(peer));
                    }
                    Some(TransportEvent::DialFailure(address)) => {
                        return Ok(Litep2pEvent::DialFailure(address));
                    }
                    None => {
                        tracing::error!(target: LOG_TARGET, "channel to transports shut down");
                        return Err(Error::EssentialTaskClosed);
                    }
                },
                event = self.libp2p_rx.recv() => match event {
                    _ => {},
                }
            }
        }
    }
}
