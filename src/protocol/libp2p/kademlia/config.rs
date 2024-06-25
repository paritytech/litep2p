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
    codec::ProtocolCodec,
    protocol::libp2p::kademlia::handle::{
        IncomingRecordValidationMode, KademliaCommand, KademliaEvent, KademliaHandle,
        RoutingTableUpdateMode,
    },
    types::protocol::ProtocolName,
    PeerId, DEFAULT_CHANNEL_SIZE,
};

use multiaddr::Multiaddr;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{collections::HashMap, time::Duration};

/// Default TTL for the records.
const DEFAULT_TTL: u64 = 36 * 60 * 60;

/// Protocol name.
const PROTOCOL_NAME: &str = "/ipfs/kad/1.0.0";

/// Kademlia replication factor.
const REPLICATION_FACTOR: usize = 20usize;

/// Kademlia configuration.
#[derive(Debug)]
pub struct Config {
    // Protocol name.
    // pub(crate) protocol: ProtocolName,
    /// Protocol names.
    pub(crate) protocol_names: Vec<ProtocolName>,

    /// Protocol codec.
    pub(crate) codec: ProtocolCodec,

    /// Replication factor.
    pub(super) replication_factor: usize,

    /// Known peers.
    pub(super) known_peers: HashMap<PeerId, Vec<Multiaddr>>,

    /// Routing table update mode.
    pub(super) update_mode: RoutingTableUpdateMode,

    /// Incoming records validation mode.
    pub(super) validation_mode: IncomingRecordValidationMode,

    /// Default record TTl.
    pub(super) record_ttl: Duration,

    /// TX channel for sending events to `KademliaHandle`.
    pub(super) event_tx: Sender<KademliaEvent>,

    /// RX channel for receiving commands from `KademliaHandle`.
    pub(super) cmd_rx: Receiver<KademliaCommand>,
}

impl Config {
    fn new(
        replication_factor: usize,
        known_peers: HashMap<PeerId, Vec<Multiaddr>>,
        mut protocol_names: Vec<ProtocolName>,
        update_mode: RoutingTableUpdateMode,
        validation_mode: IncomingRecordValidationMode,
        record_ttl: Duration,
    ) -> (Self, KademliaHandle) {
        let (cmd_tx, cmd_rx) = channel(DEFAULT_CHANNEL_SIZE);
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);

        // if no protocol names were provided, use the default protocol
        if protocol_names.is_empty() {
            protocol_names.push(ProtocolName::from(PROTOCOL_NAME));
        }

        (
            Config {
                protocol_names,
                update_mode,
                validation_mode,
                record_ttl,
                codec: ProtocolCodec::UnsignedVarint(None),
                replication_factor,
                known_peers,
                cmd_rx,
                event_tx,
            },
            KademliaHandle::new(cmd_tx, event_rx),
        )
    }

    /// Build default Kademlia configuration.
    pub fn default() -> (Self, KademliaHandle) {
        Self::new(
            REPLICATION_FACTOR,
            HashMap::new(),
            Vec::new(),
            RoutingTableUpdateMode::Automatic,
            IncomingRecordValidationMode::Automatic,
            Duration::from_secs(DEFAULT_TTL),
        )
    }
}

/// Configuration builder for Kademlia.
#[derive(Debug)]
pub struct ConfigBuilder {
    /// Replication factor.
    pub(super) replication_factor: usize,

    /// Routing table update mode.
    pub(super) update_mode: RoutingTableUpdateMode,

    /// Incoming records validation mode.
    pub(super) validation_mode: IncomingRecordValidationMode,

    /// Known peers.
    pub(super) known_peers: HashMap<PeerId, Vec<Multiaddr>>,

    /// Protocol names.
    pub(super) protocol_names: Vec<ProtocolName>,

    /// Default TTL for the records.
    pub(super) record_ttl: Duration,
}

impl Default for ConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ConfigBuilder {
    /// Create new [`ConfigBuilder`].
    pub fn new() -> Self {
        Self {
            replication_factor: REPLICATION_FACTOR,
            known_peers: HashMap::new(),
            protocol_names: Vec::new(),
            update_mode: RoutingTableUpdateMode::Automatic,
            validation_mode: IncomingRecordValidationMode::Automatic,
            record_ttl: Duration::from_secs(DEFAULT_TTL),
        }
    }

    /// Set replication factor.
    pub fn with_replication_factor(mut self, replication_factor: usize) -> Self {
        self.replication_factor = replication_factor;
        self
    }

    /// Seed Kademlia with one or more known peers.
    pub fn with_known_peers(mut self, peers: HashMap<PeerId, Vec<Multiaddr>>) -> Self {
        self.known_peers = peers;
        self
    }

    /// Set routing table update mode.
    pub fn with_routing_table_update_mode(mut self, mode: RoutingTableUpdateMode) -> Self {
        self.update_mode = mode;
        self
    }

    /// Set incoming records validation mode.
    pub fn with_incoming_records_validation_mode(
        mut self,
        mode: IncomingRecordValidationMode,
    ) -> Self {
        self.validation_mode = mode;
        self
    }

    /// Set Kademlia protocol names, overriding the default protocol name.
    ///
    /// The order of the protocol names signifies preference so if, for example, there are two
    /// protocols:
    ///  * `/kad/2.0.0`
    ///  * `/kad/1.0.0`
    ///
    /// Where `/kad/2.0.0` is the preferred version, then that should be in `protocol_names` before
    /// `/kad/1.0.0`.
    pub fn with_protocol_names(mut self, protocol_names: Vec<ProtocolName>) -> Self {
        self.protocol_names = protocol_names;
        self
    }

    /// Set default TTL for the records.
    ///
    /// If unspecified, the default TTL is 36 hours.
    pub fn with_record_ttl(mut self, record_ttl: Duration) -> Self {
        self.record_ttl = record_ttl;
        self
    }

    /// Build Kademlia [`Config`].
    pub fn build(self) -> (Config, KademliaHandle) {
        Config::new(
            self.replication_factor,
            self.known_peers,
            self.protocol_names,
            self.update_mode,
            self.validation_mode,
            self.record_ttl,
        )
    }
}
