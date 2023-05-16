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
    crypto::{ed25519::Keypair, PublicKey},
    error::Error,
    peer_id::PeerId,
    protocol::{
        libp2p::{identify, ping, Libp2pProtocolEvent},
        notification::NotificationProtocolConfig,
        request_response::RequestResponseProtocolConfig,
    },
    transport::{tcp::TcpTransport, Connection, Transport, TransportEvent},
    types::{ConnectionId, ProtocolId, RequestId},
};

use futures::{Stream, StreamExt};
use identify::IdentifyEvent;
use multiaddr::{Multiaddr, Protocol};
use protocol::TransportCommand;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot},
};
use tokio_stream::{wrappers::ReceiverStream, StreamMap};
use transport::{Direction, TransportService};

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
};

pub mod config;
pub mod protocol;

mod crypto;
mod error;
mod peer_id;
mod transport;
mod types;

// TODO: move code from `TcpTransport` to here
// TODO: remove unwraps

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

/// Validation result for inbound substream.
#[derive(Debug)]
pub enum ValidationResult {
    /// Accept the inbound substream.
    Accept,

    /// Reject the inbound substream.
    Reject,
}

#[derive(Debug)]
pub enum Litep2pEvent {
    /// Connection established.
    ConnectionEstablished(PeerId),

    /// Connection closed.
    ConnectionClosed(PeerId),

    /// Peer identified.
    PeerIdentified {
        /// Remote peer ID.
        peer: PeerId,

        /// Supported protocols.
        supported_protocols: HashSet<String>,
    },

    /// Dial failure for outbound connection.
    DialFailure(Multiaddr),

    /// Inbound and unvalidated substream received.
    InboundSubstream(String, PeerId, Vec<u8>, oneshot::Sender<ValidationResult>),

    /// Substream opened
    SubstreamOpened(String, PeerId, ()),
}

struct PeerContext {
    /// Protocols supported by the remote peer.
    protocols: HashSet<String>,
}

impl Default for PeerContext {
    fn default() -> Self {
        PeerContext {
            protocols: Default::default(),
        }
    }
}

// TODO: the storing of these handles can be greatly simplified
/// [`Litep2p`] object.
pub struct Litep2p {
    /// Enable transports.
    tranports: HashMap<&'static str, Box<dyn TransportService>>,

    /// RX channel for events received from enabled transports.
    transport_rx: mpsc::Receiver<TransportEvent>,

    /// RX channel for receiving events from libp2p standard protocols.
    libp2p_rx: mpsc::Receiver<Libp2pProtocolEvent>,

    /// TX channels for communicating with libp2p standard protocols.
    libp2p_tx: HashMap<String, mpsc::Sender<TransportEvent>>,

    /// Channels for receiving commands from request-response protocols.
    request_response_commands: StreamMap<String, ReceiverStream<TransportCommand>>,

    /// Request-response handles.
    request_response_handles: HashMap<String, mpsc::Sender<TransportEvent>>,

    /// Channels for receiving commands from request-response protocols.
    notification_commands: StreamMap<String, ReceiverStream<TransportCommand>>,

    /// Request-response handles.
    notification_handles: HashMap<String, mpsc::Sender<TransportEvent>>,

    /// Connected peers.
    peers: HashMap<PeerId, PeerContext>,

    /// Listen addresses.
    listen_addresses: HashSet<Multiaddr>,

    /// Local peer ID.
    local_peer_id: PeerId,
}

struct Litep2pBuilder {}

impl Litep2pBuilder {
    pub fn new() -> Self {
        Self {}
    }
}

impl Litep2p {
    /// Create new [`Litep2p`].
    // TODO: this code is absolutely hideous
    pub async fn new(config: LiteP2pConfiguration) -> crate::Result<Litep2p> {
        let LiteP2pConfiguration {
            listen_addresses,
            notification_protocols,
            request_response_protocols,
        } = config;

        // assert!(config.listen_addresses().count() == 1);
        let keypair = Keypair::generate();
        let local_peer_id = PeerId::from_public_key(&PublicKey::Ed25519(keypair.public()));

        tracing::debug!(target: LOG_TARGET, "local peer id: {local_peer_id}");

        // initialize protocols
        let mut protocols = vec![
            ping::PROTOCOL_NAME.to_owned(),
            identify::PROTOCOL_NAME.to_owned(),
        ];

        // initialize notification protocols
        let (notification_command_receivers, notification_handles): (Vec<_>, HashMap<_, _>) =
            notification_protocols
                .into_iter()
                .map(|config| {
                    let NotificationProtocolConfig { protocol, tx, rx } = config;
                    protocols.push(protocol.clone());

                    ((protocol.clone(), ReceiverStream::new(rx)), (protocol, tx))
                })
                .unzip();

        // initialize request-response protocols
        let (command_receivers, request_response_handles): (Vec<_>, HashMap<_, _>) =
            request_response_protocols
                .into_iter()
                .map(|config| {
                    let RequestResponseProtocolConfig { protocol, tx, rx } = config;
                    protocols.push(protocol.clone());

                    ((protocol.clone(), ReceiverStream::new(rx)), (protocol, tx))
                })
                .unzip();

        // initialize libp2p standard protocols
        let (libp2p_tx, libp2p_rx) = mpsc::channel(DEFAULT_CHANNEL_SIZE);

        let ping = ping::IpfsPing::start(libp2p_tx.clone());
        let identify = identify::IpfsIdentify::start(
            PublicKey::Ed25519(keypair.public()),
            listen_addresses.clone(),
            protocols.clone(),
            libp2p_tx.clone(),
        );

        // initialize transports
        let (transport_tx, transport_rx) = mpsc::channel(DEFAULT_CHANNEL_SIZE);

        let handle = TcpTransport::start(
            &keypair,
            TransportConfig::new(
                // TODO: what is this?
                listen_addresses.iter().next().unwrap().clone(),
                protocols,
                vec![],
                40_000,
            ),
            transport_tx,
        )
        .await?;

        Ok(Self {
            local_peer_id,
            tranports: HashMap::from([("tcp", handle)]),
            transport_rx,
            libp2p_rx,
            libp2p_tx: HashMap::from([
                (String::from(ping::PROTOCOL_NAME), ping),
                (String::from(identify::PROTOCOL_NAME), identify),
            ]),
            peers: HashMap::new(),
            request_response_commands: StreamMap::from_iter(command_receivers),
            request_response_handles,
            notification_commands: StreamMap::from_iter(notification_command_receivers),
            notification_handles,
            listen_addresses: HashSet::from_iter(listen_addresses),
        })
    }

    /// Get local peer ID.
    pub fn local_peer_id(&self) -> &PeerId {
        &self.local_peer_id
    }

    /// Open connection to remote peer at `address`.
    ///
    /// Connection is opened and negotiated in the background and the result is
    /// indicated to the caller through [`Litep2pEvent::ConnectionEstablished`]/[`Litep2pEvent::DialFailure`]
    pub async fn open_connection(&mut self, address: Multiaddr) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            ?address,
            "establish outbound connection"
        );

        if self.listen_addresses.contains(&address) {
            return Err(Error::CannotDialSelf(address));
        }

        self.tranports
            .get_mut("tcp")
            .ok_or_else(|| Error::TransportNotSupported(address.clone()))?
            .open_connection(address)
            .await;

        Ok(())
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

    /// Open notification substream to remote `peer` for `protocol`.
    ///
    /// This function doesn't block but starts the substream opening procedure.
    /// The result of the operation is polled through [`Litep2p::next_event()`] which returns
    /// [`Litep2p::SubstreamOpened`] on success and [`Litep2pEvent::SubstreamOpenFailure`] on failure.
    ///
    /// The substream is closed by dropping the sink received in [`Litep2p::SubstreamOpened`] message.
    pub async fn open_notification_substream(
        &mut self,
        protocol: String,
        peer: PeerId,
        handshake: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            ?protocol,
            ?peer,
            ?handshake,
            "open notification substream"
        );

        // verify that peer exists and supports the protocol
        if !self
            .peers
            .get(&peer)
            .ok_or(Error::PeerDoesntExist(peer))?
            .protocols
            .contains(&protocol)
        {
            return Err(Error::ProtocolNotSupported(protocol));
        }

        Ok(())
    }

    /// Send request over `protocol` to remote peer and return `oneshot::mpsc::Receiver` which can be
    /// polled for the response.
    pub async fn send_request(
        &mut self,
        protocol: String,
        peer: PeerId,
        request: Vec<u8>,
    ) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            ?protocol,
            ?peer,
            ?request,
            "send request"
        );

        // verify that peer exists and supports the protocol
        if !self
            .peers
            .get(&peer)
            .ok_or(Error::PeerDoesntExist(peer))?
            .protocols
            .contains(&protocol)
        {
            return Err(Error::ProtocolNotSupported(protocol));
        }

        Ok(())
    }

    /// Handle open substreamed.
    async fn on_substream_opened(
        &mut self,
        protocol: &String,
        peer: PeerId,
        direction: Direction,
        substream: Box<dyn Connection>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?protocol, ?peer, "substream opened");

        if let Some(tx) = self.libp2p_tx.get_mut(protocol) {
            return tx
                .send(TransportEvent::SubstreamOpened(
                    protocol.clone(),
                    peer,
                    direction,
                    substream,
                ))
                .await
                .map_err(From::from);
        }

        if let Some(tx) = self.request_response_handles.get_mut(protocol) {
            return tx
                .send(TransportEvent::SubstreamOpened(
                    protocol.clone(),
                    peer,
                    direction,
                    substream,
                ))
                .await
                .map_err(From::from);
        }

        if let Some(tx) = self.notification_handles.get_mut(protocol) {
            tracing::info!(target: LOG_TARGET, "notification substream opened");
            return tx
                .send(TransportEvent::SubstreamOpened(
                    protocol.clone(),
                    peer,
                    direction,
                    substream,
                ))
                .await
                .map_err(From::from);
        }

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

    async fn on_identify_event(&mut self, event: IdentifyEvent) -> Option<Litep2pEvent> {
        tracing::trace!(target: LOG_TARGET, ?event, "handle identify event");

        match event {
            IdentifyEvent::OpenSubstream { peer } => {
                self.tranports
                    .get_mut("tcp")
                    .unwrap()
                    .open_substream(identify::PROTOCOL_NAME.to_owned(), peer, vec![])
                    .await;
                None
            }
            IdentifyEvent::PeerIdentified {
                peer,
                supported_protocols,
            } => match self.peers.get_mut(&peer) {
                Some(context) => {
                    context.protocols.union(&supported_protocols);
                    Some(Litep2pEvent::PeerIdentified {
                        peer,
                        supported_protocols,
                    })
                }
                None => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?supported_protocols,
                        "peer information received from non-existent peer"
                    );
                    None
                }
            },
            _ => todo!(),
        }
    }

    // Handle command received from one of the `RequestResponseService`s.
    async fn on_request_response_command(&mut self, command: TransportCommand) {
        match command {
            TransportCommand::OpenSubstream { peer, protocol } => {
                self.tranports
                    .get_mut("tcp")
                    .unwrap()
                    .open_substream(protocol, peer, vec![])
                    .await;
            }
        }
    }

    // Handle command received from one of the `NotificationService`s.
    async fn on_notification_command(&mut self, command: TransportCommand) {
        match command {
            TransportCommand::OpenSubstream { peer, protocol } => {
                self.tranports
                    .get_mut("tcp")
                    .unwrap()
                    .open_substream(protocol, peer, vec![])
                    .await;
            }
        }
    }

    /// Event loop for [`Litep2p`].
    pub async fn next_event(&mut self) -> crate::Result<Litep2pEvent> {
        loop {
            tokio::select! {
                event = self.transport_rx.recv() => match event {
                    Some(TransportEvent::SubstreamOpened(protocol, peer, direction, substream)) => {
                        if let Err(err) = self.on_substream_opened(&protocol, peer, direction, substream).await {
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
                        // TODO: this needs some more thought lol
                        self.peers.insert(peer, Default::default());
                        let _ = self
                            .libp2p_tx
                            .get_mut(&identify::PROTOCOL_NAME.to_owned())
                            .unwrap()
                            .send(TransportEvent::ConnectionEstablished(peer))
                            .await;
                        return Ok(Litep2pEvent::ConnectionEstablished(peer));
                    }
                    Some(TransportEvent::ConnectionClosed(peer)) => {
                        return Ok(Litep2pEvent::ConnectionClosed(peer));
                    }
                    Some(TransportEvent::DialFailure(address)) => {
                        return Ok(Litep2pEvent::DialFailure(address));
                    }
                    Some(TransportEvent::SubstreamOpenFailure(protocol, peer, error)) => {
                        tracing::error!(target: LOG_TARGET, ?peer, ?protocol, ?error, "substream open failure");
                        // return Ok(Litep2pEvent::SubstreamOpenFailure(protocol, peer, error));
                        // todo!();
                        // tracing::error!(target: LOG_TARGET, "channel to transports shut down");
                        // return Err(Error::EssentialTaskClosed);
                    }
                    None => {
                        tracing::error!(target: LOG_TARGET, "channel to transports shut down");
                        return Err(Error::EssentialTaskClosed);
                    }
                },
                event = self.libp2p_rx.recv() => match event {
                    Some(Libp2pProtocolEvent::Identify(event)) => if let Some(event) = self.on_identify_event(event).await {
                        return Ok(event)
                    }
                    event => tracing::warn!("ignore event {event:?}"),
                },
                command = self.request_response_commands.next(), if !self.request_response_commands.is_empty() => {
                    match command {
                        Some((_, command)) => self.on_request_response_command(command).await,
                        None => {
                            tracing::error!(
                                target: LOG_TARGET,
                                "handles to request-response protocols dropped"
                            );
                            return Err(Error::EssentialTaskClosed);
                        }
                    }
                }
                command = self.notification_commands.next(), if !self.notification_commands.is_empty() => {
                    match command {
                        Some((_, command)) => self.on_notification_command(command).await,
                        None => {
                            tracing::error!(
                                target: LOG_TARGET,
                                "handles to notification protocols dropped"
                            );
                            return Err(Error::EssentialTaskClosed);
                        }
                    }
                }
            }
        }
    }
}
