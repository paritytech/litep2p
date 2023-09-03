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

//! [`/ipfs/identify/1.0.0`](https://github.com/libp2p/specs/blob/master/identify/README.md) implementation.

use crate::{
    codec::ProtocolCodec,
    crypto::PublicKey,
    error::{Error, SubstreamError},
    protocol::{Direction, Transport, TransportEvent, TransportService},
    substream::Substream,
    types::{protocol::ProtocolName, SubstreamId},
    PeerId, DEFAULT_CHANNEL_SIZE,
};

use futures::{SinkExt, Stream, StreamExt};
use multiaddr::Multiaddr;
use prost::Message;
use tokio::sync::mpsc::{channel, Sender};
use tokio_stream::wrappers::ReceiverStream;

use std::collections::{HashMap, HashSet};

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::identify";

/// IPFS Identify protocol name
const PROTOCOL_NAME: &str = "/ipfs/id/1.0.0";

/// IPFS Identify push protocol name.
const _PUSH_PROTOCOL_NAME: &str = "/ipfs/id/push/1.0.0";

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

    /// Codec used by the protocol.
    pub(crate) codec: ProtocolCodec,

    /// TX channel for sending events to the user protocol.
    tx_event: Sender<IdentifyEvent>,

    // Public key of the local node, filled by `Litep2p`.
    pub(crate) public: Option<PublicKey>,

    /// Listen addresses of the local node, filled by `Litep2p`.
    pub(crate) listen_addresses: Vec<Multiaddr>,

    /// Protocols supported by the local node, filled by `Litep2p`.
    pub(crate) protocols: Vec<ProtocolName>,
}

impl Config {
    /// Create new [`Config`].
    ///
    /// Returns a config that is given to `Litep2pConfig` and an event stream for [`IdentifyEvent`]s.
    pub fn new() -> (Self, Box<dyn Stream<Item = IdentifyEvent> + Send + Unpin>) {
        let (tx_event, rx_event) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                tx_event,
                public: None,
                codec: ProtocolCodec::UnsignedVarint(Some(IDENTIFY_PAYLOAD_SIZE)),
                listen_addresses: Vec::new(),
                protocols: Vec::new(),
                protocol: ProtocolName::from(PROTOCOL_NAME),
            },
            Box::new(ReceiverStream::new(rx_event)),
        )
    }
}

/// Events emitted by Identify protocol.
#[derive(Debug)]
pub enum IdentifyEvent {
    /// Peer identified.
    PeerIdentified {
        /// Peer ID.
        peer: PeerId,

        /// Supported protocols.
        supported_protocols: HashSet<String>,
    },
}

pub(crate) struct Identify {
    // Connection service.
    service: TransportService,

    /// TX channel for sending events to the user protocol.
    tx: Sender<IdentifyEvent>,

    /// Connected peers.
    peers: HashSet<PeerId>,

    // Public key of the local node, filled by `Litep2p`.
    public: PublicKey,

    /// Listen addresses of the local node, filled by `Litep2p`.
    listen_addresses: Vec<Multiaddr>,

    /// Protocols supported by the local node, filled by `Litep2p`.
    protocols: Vec<String>,

    /// Pending outbound substreams.
    pending_outbound: HashMap<SubstreamId, PeerId>,
}

impl Identify {
    /// Create new [`Identify`] protocol.
    pub(crate) fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            tx: config.tx_event,
            peers: HashSet::new(),
            public: config.public.expect("public key to be supplied"),
            listen_addresses: config.listen_addresses,
            pending_outbound: HashMap::new(),
            protocols: config
                .protocols
                .iter()
                .map(|protocol| protocol.to_string())
                .collect(),
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(&mut self, peer: PeerId) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection established");

        let substream_id = self.service.open_substream(peer).await?;
        self.pending_outbound.insert(substream_id, peer);

        Ok(())
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "connection closed");

        self.peers.remove(&peer);
    }

    /// Inbound substream opened.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?protocol,
            "inbound substream opened"
        );

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

        // TODO: this is not good
        substream.send(msg.into()).await
    }

    /// Outbound substream opened.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        protocol: ProtocolName,
        substream_id: SubstreamId,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?protocol,
            ?substream_id,
            "outbound substream opened"
        );

        // TODO: not good, use `SubstreamSet`
        let payload =
            substream
                .next()
                .await
                .ok_or(Error::SubstreamError(SubstreamError::ReadFailure(Some(
                    substream_id,
                ))))??;

        let info = identify_schema::Identify::decode(payload.to_vec().as_slice())?;
        self.tx
            .send(IdentifyEvent::PeerIdentified {
                peer,
                supported_protocols: HashSet::from_iter(info.protocols),
            })
            .await
            .map_err(From::from)
    }

    /// Failed to open substream to remote peer.
    fn on_substream_open_failure(&mut self, substream: SubstreamId, error: Error) {
        tracing::debug!(
            target: LOG_TARGET,
            ?substream,
            ?error,
            "failed to open substream"
        );
    }

    /// Start [`Identify`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting identify event loop");

        while let Some(event) = self.service.next_event().await {
            match event {
                TransportEvent::ConnectionEstablished { peer, .. } => {
                    if let Err(error) = self.on_connection_established(peer).await {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to register peer"
                        );
                    }
                }
                TransportEvent::ConnectionClosed { peer } => {
                    self.on_connection_closed(peer);
                }
                TransportEvent::SubstreamOpened {
                    peer,
                    protocol,
                    direction,
                    substream,
                } => match direction {
                    Direction::Inbound => {
                        if let Err(error) =
                            self.on_inbound_substream(peer, protocol, substream).await
                        {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to handle inbound substream"
                            );
                        }
                    }
                    Direction::Outbound(substream_id) => {
                        if let Err(error) = self
                            .on_outbound_substream(peer, protocol, substream_id, substream)
                            .await
                        {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?substream_id,
                                ?error,
                                "failed to handle outbound substream"
                            );
                        }
                    }
                },
                TransportEvent::SubstreamOpenFailure { substream, error } => {
                    self.on_substream_open_failure(substream, error);
                }
                _ => {}
            }
        }
    }
}
