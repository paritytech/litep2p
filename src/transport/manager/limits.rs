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

use std::collections::HashSet;

/// Configuration for the connection limits.
#[derive(Debug, Clone, Default)]
pub struct ConnectionLimitsConfig {
    /// Maximum number of incoming connections that can be established.
    max_incoming_connections: Option<usize>,
    /// Maximum number of outgoing connections that can be established.
    max_outgoing_connections: Option<usize>,
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
}

/// Error type for connection limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionLimitsError {
    /// Maximum number of incoming connections exceeded.
    MaxIncomingConnectionsExceeded,
    /// Maximum number of outgoing connections exceeded.
    MaxOutgoingConnectionsExceeded,
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

        Ok(())
    }

    /// Called when a connection is closed.
    pub fn on_connection_closed(&mut self, peer: PeerId, connection_id: ConnectionId) {
        self.incoming_connections.remove(&connection_id);
        self.outgoing_connections.remove(&connection_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{types::ConnectionId, PeerId};

    #[test]
    fn connection_limits() {
        let config = ConnectionLimitsConfig::default()
            .max_incoming_connections(Some(3))
            .max_outgoing_connections(Some(2))
            .max_connections_per_peer(Some(2));
        let mut limits = ConnectionLimits::new(config);

        let peer_a = PeerId::random();
        let peer_b = PeerId::random();
        let peer_c = PeerId::random();
        let connection_id_in_1 = ConnectionId::random();
        let connection_id_in_2 = ConnectionId::random();
        let connection_id_out_1 = ConnectionId::random();
        let connection_id_out_2 = ConnectionId::random();
        let connection_id_in_3 = ConnectionId::random();
        let connection_id_out_3 = ConnectionId::random();

        // Establish incoming connection.
        assert!(limits
            .on_connection_established(peer_a.clone(), connection_id_in_1, true)
            .is_ok());
        assert_eq!(limits.incoming_connections.len(), 1);

        assert!(limits
            .on_connection_established(peer_b.clone(), connection_id_in_2, true)
            .is_ok());
        assert_eq!(limits.incoming_connections.len(), 2);

        assert!(limits
            .on_connection_established(peer_c.clone(), connection_id_in_3, true)
            .is_ok());
        assert_eq!(limits.incoming_connections.len(), 3);

        assert_eq!(
            limits
                .on_connection_established(PeerId::random(), ConnectionId::random(), true)
                .unwrap_err(),
            ConnectionLimitsError::MaxIncomingConnectionsExceeded
        );
        assert_eq!(limits.incoming_connections.len(), 3);

        // Establish outgoing connection.
        assert!(limits
            .on_connection_established(peer_a.clone(), connection_id_out_1, false)
            .is_ok());
        assert_eq!(limits.incoming_connections.len(), 3);
        assert_eq!(limits.outgoing_connections.len(), 1);

        assert!(limits
            .on_connection_established(peer_b.clone(), connection_id_out_2, false)
            .is_ok());
        assert_eq!(limits.incoming_connections.len(), 3);
        assert_eq!(limits.outgoing_connections.len(), 2);

        assert_eq!(
            limits
                .on_connection_established(peer_c.clone(), connection_id_out_3, false)
                .unwrap_err(),
            ConnectionLimitsError::MaxOutgoingConnectionsExceeded
        );

        // Close connections with peer a.
        limits.on_connection_closed(peer_a.clone(), connection_id_in_1);
        assert_eq!(limits.incoming_connections.len(), 2);
        assert_eq!(limits.outgoing_connections.len(), 2);
        assert_eq!(limits.connections_per_peer.len(), 3);

        limits.on_connection_closed(peer_a.clone(), connection_id_out_1);
        assert_eq!(limits.incoming_connections.len(), 2);
        assert_eq!(limits.outgoing_connections.len(), 1);
        assert_eq!(limits.connections_per_peer.len(), 2);

        // We have room for another incoming connection, however the limit for peer b is reached.
        assert_eq!(
            limits
                .on_connection_established(peer_b.clone(), connection_id_in_3, false)
                .unwrap_err(),
            ConnectionLimitsError::MaxConnectionsPerPeerExceeded
        );
    }
}
