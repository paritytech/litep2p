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
    error::Error,
    peer_id::PeerId,
    protocol::{
        libp2p::kademlia::{
            handle::{KademliaCommand, KademliaEvent},
            key::Key,
            message::KademliaMessage,
        },
        ConnectionEvent, ConnectionService, Direction,
    },
    substream::{Substream, SubstreamSet},
    transport::TransportService,
    types::SubstreamId,
};

use bytes::BytesMut;
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc::{Receiver, Sender};

use std::collections::{hash_map::Entry, HashMap};

pub use crate::protocol::libp2p::kademlia::config::{Config, ConfigBuilder};

/// Logging target for the file.
const LOG_TARGET: &str = "ipfs::kademlia";

mod config;
mod handle;
mod key;
mod message;
mod store;
mod types;

mod schema {
    pub(super) mod kademlia {
        include!(concat!(env!("OUT_DIR"), "/kademlia.rs"));
    }
}

/// Peer context.
struct PeerContext {
    /// Connection service for the peer.
    service: ConnectionService,
}

impl PeerContext {
    /// Create new [`PeerContext`].
    fn new(service: ConnectionService) -> Self {
        Self { service }
    }
}

/// Main Kademlia object.
pub struct Kademlia {
    /// Transport service.
    service: TransportService,

    /// Local Kademlia key.
    local_key: Key<PeerId>,

    /// Connected peers,
    peers: HashMap<PeerId, PeerContext>,

    /// Substream set.
    substreams: SubstreamSet<PeerId>,

    /// TX channel for sending events to `KademliaHandle`.
    event_tx: Sender<KademliaEvent>,

    /// RX channel for receiving commands from `KademliaHandle`.
    cmd_rx: Receiver<KademliaCommand>,
}

impl Kademlia {
    /// Create new [`Kademlia`].
    pub fn new(service: TransportService, config: Config) -> Self {
        let local_key = Key::from(service.local_peer_id());

        Self {
            service,
            local_key,
            peers: HashMap::new(),
            substreams: SubstreamSet::new(),
            event_tx: config.event_tx,
            cmd_rx: config.cmd_rx,
        }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(
        &mut self,
        peer: PeerId,
        mut service: ConnectionService,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        let Entry::Vacant(entry) = self.peers.entry(peer) else {
            return Err(Error::PeerAlreadyExists(peer));
        };

        match service.open_substream().await {
            Ok(substream_id) => {
                entry.insert(PeerContext::new(service));
                Ok(())
            }
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    ?error,
                    "failed to open substream to remote peer"
                );
                return Err(error);
            }
        }
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        if let None = self.peers.remove(&peer) {
            tracing::debug!(target: LOG_TARGET, ?peer, "peer doesn't exist");
        }
    }

    /// Local node opened a substream to remote node.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        _substream_id: SubstreamId,
        mut substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "outbound substream opened");

        let message = KademliaMessage::find_node(PeerId::random());

        match substream.send(message.into()).await {
            Ok(res) => {
                tracing::info!(
                    target: LOG_TARGET,
                    ?peer,
                    ?res,
                    "`FIND_NODE` message sent to peer"
                );
                self.substreams.insert(peer, substream);
            }
            Err(error) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    ?error,
                    "failed to send `FIND_NODE` message to peer"
                );
            }
        }

        Ok(())
    }

    /// Remote opened a substream to local node.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        _substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "inbound substream opened");

        Ok(())
    }

    fn on_inbound_message(&mut self, peer: PeerId, message: BytesMut) {
        match KademliaMessage::from_bytes(message) {
            Some(KademliaMessage::FindNode { peers }) => {
                for peer in peers {
                    tracing::info!(
                        target: LOG_TARGET,
                        peer_id = ?peer.peer,
                        addresses = ?peer.addresses,
                        connection = ?peer.connection
                    );
                }
            }
            _ => tracing::debug!(target: LOG_TARGET, "ignoring unsupported message type"),
        }
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

    pub async fn run(mut self) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, "starting kademlia event loop");

        loop {
            tokio::select! {
                event = self.service.next_event() => match event {
                    Some(ConnectionEvent::ConnectionEstablished { peer, service }) => {
                        if let Err(error) = self.on_connection_established(peer, service).await {
                            tracing::debug!(target: LOG_TARGET, ?error, "failed to handle established connection");
                        }
                    }
                    Some(ConnectionEvent::ConnectionClosed { peer }) => {
                        self.on_connection_closed(peer);
                    }
                    Some(ConnectionEvent::SubstreamOpened { peer, direction, substream, .. }) => {
                        match direction {
                            Direction::Inbound => {
                                if let Err(error) = self.on_inbound_substream(peer, substream).await {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?error,
                                        "failed to handle inbound substream",
                                    );
                                }
                            }
                            Direction::Outbound(substream_id) => {
                                if let Err(error) = self.on_outbound_substream(peer, substream_id, substream).await {
                                    tracing::debug!(
                                        target: LOG_TARGET,
                                        ?peer,
                                        ?substream_id,
                                        ?error,
                                        "failed to handle outbound substream",
                                    );
                                }
                            }
                        }
                    },
                    Some(ConnectionEvent::SubstreamOpenFailure { substream, error }) => {
                        self.on_substream_open_failure(substream, error);
                    }
                    None => return Ok(()),
                },
                command = self.cmd_rx.recv() => match command {
                    Some(_) => {}
                    None => {}
                },
                event = self.substreams.next() => match event {
                    Some((peer, message)) => match message {
                        Ok(message) => self.on_inbound_message(peer, message),
                        Err(error) => tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to read message"),
                    },
                    None => todo!(),
                }
            }
        }
    }
}
