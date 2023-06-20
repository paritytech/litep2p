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
    codec::{identity::Identity, Codec},
    crypto::{ed25519::Keypair, PublicKey},
    new::ConnectionService,
    peer_id::PeerId,
    protocol::{ConnectionEvent, ProtocolEvent},
    substream::{Substream, SubstreamSet},
    types::protocol::ProtocolName,
    DEFAULT_CHANNEL_SIZE,
};

use asynchronous_codec::{Framed, FramedParts};
use bytes::BytesMut;
use futures::{AsyncReadExt, AsyncWriteExt, SinkExt, Stream, StreamExt};
use multiaddr::Multiaddr;
use prost::Message;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use unsigned_varint::{
    codec::UviBytes,
    decode::{self, Error},
    encode,
};

use std::collections::{HashMap, HashSet};

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::identify";

/// IPFS Identify protocol name
pub const PROTOCOL_NAME: &str = "/ipfs/id/1.0.0";

/// IPFS Identify push protocol name.
pub const PUSH_PROTOCOL_NAME: &str = "/ipfs/id/push/1.0.0";

/// Size for `/ipfs/ping/1.0.0` payloads.
// TODO: what is the max size?
const IDENTIFY_PAYLOAD_SIZE: usize = 4096;

mod identify_schema {
    include!(concat!(env!("OUT_DIR"), "/identify.rs"));
}

/// Identify configuration.
#[derive(Debug)]
pub struct Config {
    /// Protocol name.
    pub(crate) protocol: ProtocolName,

    /// TX channel for sending events to the user protocol.
    tx_event: Sender<IdentifyEvent>,

    // Public key of the local node, filled by `Litep2p`.
    public: Option<PublicKey>,

    /// Listen addresses of the local node, filled by `Litep2p`.
    listen_addresses: Vec<Multiaddr>,

    /// Protocols supported by the local node, filled by `Litep2p`.
    protocols: Vec<String>,
}

impl Config {
    /// Create new [`IdentifyConfig`].
    ///
    /// Returns a config that is given to `Litep2pConfig` and an event stream for ping events.
    pub fn new() -> (Self, Box<dyn Stream<Item = IdentifyEvent> + Send>) {
        let (tx_event, rx_event) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                tx_event,
                protocol: ProtocolName::from(PROTOCOL_NAME),
                public: None,
                listen_addresses: Vec::new(),
                protocols: Vec::new(),
            },
            Box::new(ReceiverStream::new(rx_event)),
        )
    }
}

/// Events emitted by `IpfsIdentify`.
#[derive(Debug)]
pub enum IdentifyEvent {
    /// Peer identified.
    PeerIdentified {
        /// Remote peer ID.
        peer: PeerId,

        /// Supported protocols.
        supported_protocols: HashSet<String>,
    },
}

pub struct Identify {
    // Connection service.
    service: ConnectionService,

    /// TX channel for sending events to the user protocol.
    tx: Sender<IdentifyEvent>,

    /// Connected peers.
    peers: HashMap<PeerId, Sender<ProtocolEvent>>,

    // Public key of the local node, filled by `Litep2p`.
    public: Option<PublicKey>,

    /// Listen addresses of the local node, filled by `Litep2p`.
    listen_addresses: Vec<Multiaddr>,

    /// Protocols supported by the local node, filled by `Litep2p`.
    protocols: Vec<String>,
}

impl Identify {
    /// Create new [`Identify`] protocol.
    pub fn new(service: ConnectionService, config: Config) -> Self {
        Self {
            service,
            tx: config.tx_event,
            peers: HashMap::new(),
            public: config.public,
            listen_addresses: config.listen_addresses,
            protocols: config.protocols,
        }
    }

    /// Start [`Identify`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting ping event loop");

        while let Some(event) = self.service.next_event().await {
            match event {
                ConnectionEvent::ConnectionEstablished { peer, connection } => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "connection established");
                    self.peers.insert(peer, connection);
                }
                ConnectionEvent::ConnectionClosed { peer } => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "connection closed");
                    self.peers.remove(&peer);
                }
                ConnectionEvent::SubstreamOpened {
                    peer,
                    protocol,
                    mut substream,
                } => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "substream opened");
                    // TODO: handle open substream
                }
                ConnectionEvent::SubstreamOpenFailure { peer, error } => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?error,
                        "failed to open substream"
                    );
                }
            }
        }
    }
}

/*
pub struct IpfsIdentify {
    /// TX channel for sending `Libp2pProtocolEvent`s.
    event_tx: Sender<Libp2pProtocolEvent>,

    /// RX channel for receiving `TransportEvent`s.
    transport_rx: Receiver<TransportEvent>,

    /// Read buffer for incoming messages.
    read_buffer: Vec<u8>,

    /// Public key of the local node.
    public: PublicKey,

    /// Listen addresses of the local node.
    listen_addresses: Vec<Multiaddr>,

    /// Protocols supported by the local node.
    protocols: Vec<String>,
}

impl IpfsIdentify {
    /// Create new [`IpfsPing`] object and start its event loop.
    pub fn start(
        public: PublicKey,
        listen_addresses: Vec<Multiaddr>,
        protocols: Vec<String>,
        event_tx: Sender<Libp2pProtocolEvent>,
    ) -> Sender<TransportEvent> {
        let (transport_tx, transport_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let identify = Self {
            public,
            listen_addresses,
            protocols,
            event_tx,
            transport_rx,
            read_buffer: vec![0u8; IDENTIFY_PAYLOAD_SIZE],
        };

        tokio::spawn(identify.run());
        transport_tx
    }

    /// Handle inbound query by answering to it with local node's information.
    async fn handle_inbound_query(&mut self, substream: Box<dyn Connection>) {
        tracing::trace!(target: LOG_TARGET, "handle inbound stream");

        // TODO: fill with proper info
        let identify = identify_schema::Identify {
            protocol_version: None,
            agent_version: None,
            public_key: Some(self.public.to_protobuf_encoding()),
            listen_addrs: self
                .listen_addresses
                .iter()
                .map(|address| address.to_vec())
                .collect::<Vec<_>>(),
            observed_addr: None, // TODO: fill this at some point
            protocols: self.protocols.clone(),
        };
        let mut msg = Vec::with_capacity(identify.encoded_len());
        identify
            .encode(&mut msg)
            .expect("`msg` to have enough capacity");

        // why is this using unsigned-varint?
        let mut decoder = Framed::new(substream, UviBytes::<bytes::Bytes>::default());
        decoder.send(BytesMut::from(&msg[..]).into()).await.unwrap();
    }

    /// Handle outbound query by reading the remote node's information.
    async fn handle_outbound_query(&mut self, peer: PeerId, substream: Box<dyn Connection>) {
        tracing::trace!(target: LOG_TARGET, "handle outbound stream");

        // why is this using unsigned-varint?
        let mut decoder = Framed::new(substream, UviBytes::<bytes::Bytes>::default());
        let payload = decoder.next().await.unwrap().unwrap().freeze();

        match identify_schema::Identify::decode(payload.to_vec().as_slice()) {
            Ok(identify) => {
                let _ = self
                    .event_tx
                    .send(Libp2pProtocolEvent::Identify(
                        IdentifyEvent::PeerIdentified {
                            peer,
                            supported_protocols: HashSet::from_iter(identify.protocols),
                        },
                    ))
                    .await;
            }
            Err(err) => {
                tracing::error!(
                    target: LOG_TARGET,
                    "failed to parse `Identify` from response"
                );
            }
        }
    }

    /// [`IpfsIdentify`] event loop.
    async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "start ipfs ping event loop");

        while let Some(event) = self.transport_rx.recv().await {
            match event {
                TransportEvent::SubstreamOpened(protocol, peer, direction, mut substream) => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "ipfs identify substream opened");

                    match direction {
                        Direction::Outbound => self.handle_outbound_query(peer, substream).await,
                        Direction::Inbound => self.handle_inbound_query(substream).await,
                    }
                }
                TransportEvent::ConnectionEstablished(peer) => {
                    tracing::trace!(target: LOG_TARGET, target = ?peer, "initiate identify query");
                    if let Err(err) = self
                        .event_tx
                        .send(Libp2pProtocolEvent::Identify(
                            IdentifyEvent::OpenSubstream { peer },
                        ))
                        .await
                    {
                        tracing::debug!(
                            target: LOG_TARGET,
                            "channel to `litep2p` closed, closing identify"
                        );
                        return;
                    }
                }
                event => {
                    tracing::info!(target: LOG_TARGET, ?event, "ignoring `TransportEvent`");
                }
            }
        }
    }
}
*/
