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

use crate::{peer_id::PeerId, protocol::libp2p::kademlia::record::Key};

use multiaddr::Multiaddr;
use tokio::sync::mpsc::{Receiver, Sender};

/// Logging target for the file.
const LOG_TARGET: &str = "ifsp::kademlia::handle";

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

    /// Store value to DHT.
    PutValue {
        /// Key.
        key: Key,

        /// Value.
        value: Vec<u8>,
    },
}

/// Kademlia events.
#[derive(Debug, Clone)]
pub enum KademliaEvent {}

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

    /// Store value to DHT.
    pub async fn put_value(&self, key: Key, value: Vec<u8>) {
        let _ = self
            .cmd_tx
            .send(KademliaCommand::PutValue { key, value })
            .await;
    }

    /// Poll next event from [`Kademlia`].
    pub async fn next_event(&mut self) -> Option<KademliaEvent> {
        self.event_rx.recv().await
    }
}
