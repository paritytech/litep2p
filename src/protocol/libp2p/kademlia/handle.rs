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
    protocol::libp2p::kademlia::{QueryId, Record, RecordKey},
    PeerId,
};

use futures::Stream;
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{Receiver, Sender};

use std::{
    collections::HashMap,
    num::NonZeroUsize,
    pin::Pin,
    task::{Context, Poll},
};

/// Quorum.
///
/// Quorum defines how many peers must be successfully contacted
/// in order for the query to be considered successful.
#[derive(Debug, Copy, Clone)]
pub enum Quorum {
    /// All peers must be successfully contacted.
    All,

    /// One peer must be successfully contacted.
    One,

    /// `N` peer must be successfully contacted.
    N(NonZeroUsize),
}

/// Routing table update mode.
#[derive(Debug, Copy, Clone)]
pub enum RoutingTableUpdateMode {
    /// Don't insert discovered peers automatically to the routing tables but
    /// allow user to do that by calling [`KademliaHandle::add_known_peer()`].
    Manual,

    /// Automatically add all discovered peers to routing tables.
    Automatic,
}

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

        /// Query ID for the query.
        query_id: QueryId,
    },

    /// Store record to DHT.
    PutRecord {
        /// Record.
        record: Record,

        /// Query ID for the query.
        query_id: QueryId,
    },

    /// Store record to DHT to the given peers.
    ///
    /// Similar to [`KademliaCommand::PutRecord`] but allows user to specify the peers.
    PutRecordToPeers {
        /// Record.
        record: Record,

        /// Query ID for the query.
        query_id: QueryId,

        /// Use the following peers for the put request.
        peers: Vec<PeerId>,

        /// Update local store.
        update_local_store: bool,
    },

    /// Get record from DHT.
    GetRecord {
        /// Record key.
        key: RecordKey,

        /// [`Quorum`] for the query.
        quorum: Quorum,

        /// Query ID for the query.
        query_id: QueryId,
    },
}

/// Kademlia events.
#[derive(Debug, Clone)]
pub enum KademliaEvent {
    /// Result for the issued `FIND_NODE` query.
    FindNodeSuccess {
        /// Query ID.
        query_id: QueryId,

        /// Target of the query
        target: PeerId,

        /// Found nodes and their addresses.
        peers: Vec<(PeerId, Vec<Multiaddr>)>,
    },

    /// Routing table update.
    ///
    /// Kademlia has discovered one or more peers that should be added to the routing table.
    /// If [`RoutingTableUpdateMode`] is `Automatic`, user can ignore this event unless some
    /// upper-level protocols has user for this information.
    ///
    /// If the mode was set to `Manual`, user should call [`KademliaHandle::add_known_peer()`]
    /// in order to add the peers to routing table.
    RoutingTableUpdate {
        /// Discovered peers.
        peers: Vec<PeerId>,
    },

    /// `GET_VALUE` query succeeded.
    GetRecordSuccess {
        /// Query ID.
        query_id: QueryId,

        /// Found records.
        records: RecordsType,
    },

    /// `PUT_VALUE` query succeeded.
    PutRecordSucess {
        /// Query ID.
        query_id: QueryId,

        /// Record key.
        key: RecordKey,
    },

    /// Query failed.
    QueryFailed {
        /// Query ID.
        query_id: QueryId,
    },
}

/// The type of the DHT records.
#[derive(Debug, Clone)]
pub enum RecordsType {
    /// Record was found in the local store.
    ///
    /// This contains only a single result.
    LocalStore(Record),

    /// Records found in the network.
    Network(HashMap<Record, Vec<PeerId>>),
}

/// Handle for communicating with the Kademlia protocol.
pub struct KademliaHandle {
    /// TX channel for sending commands to `Kademlia`.
    cmd_tx: Sender<KademliaCommand>,

    /// RX channel for receiving events from `Kademlia`.
    event_rx: Receiver<KademliaEvent>,

    /// Next query ID.
    next_query_id: usize,
}

impl KademliaHandle {
    /// Create new [`KademliaHandle`].
    pub(super) fn new(cmd_tx: Sender<KademliaCommand>, event_rx: Receiver<KademliaEvent>) -> Self {
        Self {
            cmd_tx,
            event_rx,
            next_query_id: 0usize,
        }
    }

    /// Allocate next query ID.
    fn next_query_id(&mut self) -> QueryId {
        let query_id = self.next_query_id;
        self.next_query_id += 1;

        QueryId(query_id)
    }

    /// Add known peer.
    pub async fn add_known_peer(&self, peer: PeerId, addresses: Vec<Multiaddr>) {
        let _ = self.cmd_tx.send(KademliaCommand::AddKnownPeer { peer, addresses }).await;
    }

    /// Send `FIND_NODE` query to known peers.
    pub async fn find_node(&mut self, peer: PeerId) -> QueryId {
        let query_id = self.next_query_id();
        let _ = self.cmd_tx.send(KademliaCommand::FindNode { peer, query_id }).await;

        query_id
    }

    /// Store record to DHT.
    pub async fn put_record(&mut self, record: Record) -> QueryId {
        let query_id = self.next_query_id();
        let _ = self.cmd_tx.send(KademliaCommand::PutRecord { record, query_id }).await;

        query_id
    }

    /// Store record to DHT to the given peers.
    pub async fn put_record_to_peers(
        &mut self,
        record: Record,
        peers: Vec<PeerId>,
        update_local_store: bool,
    ) -> QueryId {
        let query_id = self.next_query_id();
        let _ = self
            .cmd_tx
            .send(KademliaCommand::PutRecordToPeers {
                record,
                query_id,
                peers,
                update_local_store,
            })
            .await;

        query_id
    }

    /// Get record from DHT.
    pub async fn get_record(&mut self, key: RecordKey, quorum: Quorum) -> QueryId {
        let query_id = self.next_query_id();
        let _ = self
            .cmd_tx
            .send(KademliaCommand::GetRecord {
                key,
                quorum,
                query_id,
            })
            .await;

        query_id
    }

    /// Try to add known peer and if the channel is clogged, return an error.
    pub fn try_add_known_peer(&self, peer: PeerId, addresses: Vec<Multiaddr>) -> Result<(), ()> {
        self.cmd_tx
            .try_send(KademliaCommand::AddKnownPeer { peer, addresses })
            .map_err(|_| ())
    }

    /// Try to initiate `FIND_NODE` query and if the channel is clogged, return an error.
    pub fn try_find_node(&mut self, peer: PeerId) -> Result<QueryId, ()> {
        let query_id = self.next_query_id();
        self.cmd_tx
            .try_send(KademliaCommand::FindNode { peer, query_id })
            .map(|_| query_id)
            .map_err(|_| ())
    }

    /// Try to initiate `PUT_VALUE` query and if the channel is clogged, return an error.
    pub fn try_put_record(&mut self, record: Record) -> Result<QueryId, ()> {
        let query_id = self.next_query_id();
        self.cmd_tx
            .try_send(KademliaCommand::PutRecord { record, query_id })
            .map(|_| query_id)
            .map_err(|_| ())
    }

    /// Try to initiate `PUT_VALUE` query to the given peers and if the channel is clogged,
    /// return an error.
    pub fn try_put_record_to_peers(
        &mut self,
        record: Record,
        peers: Vec<PeerId>,
        update_local_store: bool,
    ) -> Result<QueryId, ()> {
        let query_id = self.next_query_id();
        self.cmd_tx
            .try_send(KademliaCommand::PutRecordToPeers {
                record,
                query_id,
                peers,
                update_local_store,
            })
            .map(|_| query_id)
            .map_err(|_| ())
    }

    /// Try to initiate `GET_VALUE` query and if the channel is clogged, return an error.
    pub fn try_get_record(&mut self, key: RecordKey, quorum: Quorum) -> Result<QueryId, ()> {
        let query_id = self.next_query_id();
        self.cmd_tx
            .try_send(KademliaCommand::GetRecord {
                key,
                quorum,
                query_id,
            })
            .map(|_| query_id)
            .map_err(|_| ())
    }
}

impl Stream for KademliaHandle {
    type Item = KademliaEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.event_rx.poll_recv(cx)
    }
}
