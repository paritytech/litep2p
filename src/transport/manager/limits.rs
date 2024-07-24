// Copyright 2024 litep2p developers
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

//! Limits for the transport manager.

use crate::{types::ConnectionId, PeerId};

use std::collections::{HashMap, HashSet};

/// Configuration for the connection limits.
#[derive(Debug, Clone, Default)]
pub struct ConnectionLimitsConfig {
    /// Maximum number of incoming connections that can be established.
    max_incoming_connections: Option<usize>,
    /// Maximum number of outgoing connections that can be established.
    max_outgoing_connections: Option<usize>,
    /// Maximum number of connections that can be established per peer.
    max_connections_per_peer: Option<usize>,
}

impl ConnectionLimitsConfig {
    /// Configures the maximum number of incoming connections that can be established.
    pub fn max_incoming_connections(mut self, limit: Option<usize>) -> Self {
        self.max_incoming_connections = limit;
        self
    }

    /// Configures the maximum number of outgoing connections that can be established.
    pub fn max_outgoing_connections(mut self, limit: Option<usize>) -> Self {
        self.max_outgoing_connections = limit;
        self
    }

    /// Configures the maximum number of connections that can be established per peer.
    pub fn max_connections_per_peer(mut self, limit: Option<usize>) -> Self {
        self.max_connections_per_peer = limit;
        self
    }
}

/// Error type for connection limits.
enum ConnectionLimitsError {
    /// Maximum number of incoming connections exceeded.
    MaxIncomingConnectionsExceeded,
    /// Maximum number of outgoing connections exceeded.
    MaxOutgoingConnectionsExceeded,
    /// Maximum number of connections per peer exceeded.
    MaxConnectionsPerPeerExceeded,
}

/// Connection limits.
#[derive(Debug, Clone)]
pub struct ConnectionLimits {
    /// Configuration for the connection limits.
    config: ConnectionLimitsConfig,

    /// Established incoming connections.
    incoming_connections: HashSet<ConnectionId>,
    /// Established outgoing connections.
    outgoing_connections: HashSet<ConnectionId>,
    /// Maximum number of connections that can be established per peer.
    connections_per_peer: HashMap<PeerId, HashSet<ConnectionId>>,
}

impl ConnectionLimits {
    /// Creates a new connection limits instance.
    pub fn new(config: ConnectionLimitsConfig) -> Self {
        let max_incoming_connections = config.max_incoming_connections.unwrap_or(0);
        let max_outgoing_connections = config.max_outgoing_connections.unwrap_or(0);

        Self {
            config,
            incoming_connections: HashSet::with_capacity(max_incoming_connections),
            outgoing_connections: HashSet::with_capacity(max_outgoing_connections),
            connections_per_peer: HashMap::new(),
        }
    }

    /// Called when a new connection is established.
    pub fn on_connection_established(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
        is_listener: bool,
    ) -> Result<(), ConnectionLimitsError> {
        // Check connection limits.
        if is_listener {
            if let Some(max_incoming_connections) = self.config.max_incoming_connections {
                if self.incoming_connections.len() >= max_incoming_connections {
                    return Err(ConnectionLimitsError::MaxIncomingConnectionsExceeded);
                }
            }
        } else {
            if let Some(max_outgoing_connections) = self.config.max_outgoing_connections {
                if self.outgoing_connections.len() >= max_outgoing_connections {
                    return Err(ConnectionLimitsError::MaxOutgoingConnectionsExceeded);
                }
            }
        }

        if let Some(max_connections_per_peer) = self.config.max_connections_per_peer {
            if let Some(connections) = self.connections_per_peer.get(&peer) {
                if connections.len() >= max_connections_per_peer {
                    return Err(ConnectionLimitsError::MaxConnectionsPerPeerExceeded);
                }
            }
        }

        // Keep track of the connection.
        if is_listener {
            if self.config.max_incoming_connections.is_some() {
                self.incoming_connections.insert(connection_id);
            }
        } else {
            if self.config.max_outgoing_connections.is_some() {
                self.outgoing_connections.insert(connection_id);
            }
        }

        if let Some(max_connections_per_peer) = self.config.max_connections_per_peer {
            self.connections_per_peer
                .entry(peer)
                .or_insert_with(|| HashSet::with_capacity(max_connections_per_peer))
                .insert(connection_id);
        }

        Ok(())
    }

    /// Called when a connection is closed.
    pub fn on_connection_closed(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
        is_listener: bool,
    ) {
        if is_listener {
            self.incoming_connections.remove(&connection_id);
        } else {
            self.outgoing_connections.remove(&connection_id);
        }
        if let Some(connections) = self.connections_per_peer.get_mut(&peer) {
            connections.remove(&connection_id);
        }
    }
}
