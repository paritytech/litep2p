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
    peer_id::PeerId,
    protocol::libp2p::kademlia::{Record, RecordKey},
};

use futures::Stream;
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{Receiver, Sender};

use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// Logging target for the file.
const _LOG_TARGET: &str = "ifsp::kademlia::handle";

/// Kademlia commands.
#[derive(Debug)]
pub(crate) enum KademliaCommand {
    /// Add known peer.
    AddKnownPeer {
        /// Peer ID.
        peer: PeerId,

        /// Addresses of peer.
        addresses: Vec<Multiaddr>,
    },

    /// Send `FIND_NODE` message.
    FindNode {
        /// Peer ID.
        peer: PeerId,
    },

    /// Store record to DHT.
    PutRecord {
        /// Record.
        record: Record,
    },

    /// Get record from DHT.
    GetRecord {
        /// Record key.
        key: RecordKey,
    },
}

/// Kademlia events.
#[derive(Debug, Clone)]
pub enum KademliaEvent {
    /// Result for the issued `FIND_NODE` query.
    FindNodeResult {
        /// Target of the query
        target: PeerId,

        /// Found nodes and their addresses.
        peers: Vec<(PeerId, Vec<Multiaddr>)>,
    },
}

/// Handle for communicating with `Kademlia`.
pub struct KademliaHandle {
    /// TX channel for sending commands to `Kademlia`.
    cmd_tx: Sender<KademliaCommand>,

    /// RX channel for receiving events from `Kademlia`.
    event_rx: Receiver<KademliaEvent>,
}

impl KademliaHandle {
    /// Create new [`KademliaHandle`].
    pub(super) fn new(cmd_tx: Sender<KademliaCommand>, event_rx: Receiver<KademliaEvent>) -> Self {
        Self { cmd_tx, event_rx }
    }

    /// Add known peer.
    pub async fn add_known_peer(&self, peer: PeerId, addresses: Vec<Multiaddr>) {
        let _ = self
            .cmd_tx
            .send(KademliaCommand::AddKnownPeer { peer, addresses })
            .await;
    }

    /// Send `FIND_NODE` query to known peers.
    pub async fn find_node(&mut self, peer: PeerId) {
        let _ = self.cmd_tx.send(KademliaCommand::FindNode { peer }).await;
    }

    /// Store record to DHT.
    pub async fn put_record(&mut self, record: Record) {
        let _ = self
            .cmd_tx
            .send(KademliaCommand::PutRecord { record })
            .await;
    }

    /// Get record from DHT.
    pub async fn get_record(&mut self, key: RecordKey) {
        let _ = self.cmd_tx.send(KademliaCommand::GetRecord { key }).await;
    }
}

impl Stream for KademliaHandle {
    type Item = KademliaEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.event_rx.poll_recv(cx)
    }
}
