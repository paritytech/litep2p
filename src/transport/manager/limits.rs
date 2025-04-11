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

use crate::{transport::Endpoint, types::ConnectionId, PeerId};

use std::{collections::HashSet, net::SocketAddr};

/// A middleware trait for managing connections.
///
/// This middleware allows developers to implement custom connection policies,
/// enabling a wide range of use cases by exposing hooks into the connection lifecycle.
///
/// It interacts with the transport manager at two stages:
///
/// ## 1. Before Negotiation
///
/// At this stage, the connection has not yet been negotiated. In the context of litep2p,
/// "negotiation" refers to the handshake and setup of `crypto/noise` (encryption and peer ID
/// validation) and `yamux` (multiplexing).
///
/// The node is either attempting to establish an outbound connection or accept an inbound one.
///
/// - Returning an error here will prevent the negotiation from proceeding, saving resources.
///
/// - [`Self::outbound_capacity`] is called to determine the number of outbound
///  connections that can be established. The peerID is provided to further provide connection
///  details.
///
/// - [`Self::check_inbound`] is called to evaluate whether an inbound connection can be accepted.
///  The peer ID is not yet known, but the socket address is provided to identify the connection.
///
/// ## 2. After Negotiation
///
/// At this point, the connection has been successfully negotiated and the peer ID is known.
///
/// - [`Self::can_accept_connection`] is invoked to determine if the fully negotiated connection
///   should be accepted. The peer ID, endpoint, and connection ID are provided. Implementations
///   should check internal limits but **must not** store the connection ID or endpoint here, as the
///   transport manager might still reject the connection later.
///
/// - If the connection is accepted, [`Self::on_connection_established`] is called with the same
///   peer ID and endpoint. At this point, implementations should begin tracking the connection ID.
///
/// - When a connection is closed, [`Self::on_connection_closed`] is called. Implementations must
///   clean up any resources associated with the connection ID to prevent memory leaks.
pub trait ConnectionMiddleware: Send {
    /// Determines the number of outbound connections permitted to be established.
    ///
    /// This method is called before the node attempts to dial a remote peer.
    ///
    /// Returns the number of allowed outbound connections.
    /// - If there is no limit, returns `Ok(usize::MAX)`.
    /// - If the node cannot accept any more outbound connections, returns an error.
    fn outbound_capacity(&mut self, peer: PeerId) -> crate::Result<usize>;

    /// Checks whether a new inbound connection can be accepted before processing it.
    ///
    /// At this point, no protocol negotiation has occurred and the peer identity is
    /// unknown. The connection ID provided is the one that will be used for the
    /// connection.
    fn check_inbound(
        &mut self,
        connection_id: ConnectionId,
        address: SocketAddr,
    ) -> crate::Result<()>;

    /// Verifies if a new connection (inbound or outbound) can be established.
    ///
    /// Returns an error if connection limits or policy constraints prevent
    /// establishing the connection.
    ///
    /// # Note
    ///
    /// This method is called before the connection is established. However,
    /// the transport manager can decide to reject the connection even if this
    /// method returns `Ok(())`. Therefore, the API makes no guarantees of
    /// further calling [`Self::on_connection_established`].
    ///
    /// Implementations should inspect the provided parameters. To avoid leaking
    /// memory, the implementation should not store the connection ID or endpoint
    /// at this point in time.
    fn can_accept_connection(&mut self, peer: PeerId, endpoint: &Endpoint) -> crate::Result<()>;

    /// Registers a connection as established.
    ///
    /// This method will be called after a successful check using [`Self::can_accept_connection`].
    /// The peer ID and endpoint are provided to identify the connection and are identical
    /// to the ones used in [`Self::can_accept_connection`].
    fn on_connection_established(&mut self, peer: PeerId, endpoint: &Endpoint);

    /// Deregisters a connection when it is closed.
    ///
    /// This method will be called after a [`Self::on_connection_established`] call.
    /// The connection ID corresponds the endpoint provided in the
    /// [`Self::on_connection_established`] method.
    fn on_connection_closed(&mut self, peer: PeerId, connection_id: ConnectionId);
}

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

/// General connection limits.
///
/// This is a type of connection middleware that places limits on the number
/// of incoming and outgoing connections.
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
}

impl ConnectionMiddleware for ConnectionLimits {
    fn outbound_capacity(&mut self, _peer: PeerId) -> crate::Result<usize> {
        if let Some(max_outgoing_connections) = self.config.max_outgoing_connections {
            if self.outgoing_connections.len() >= max_outgoing_connections {
                return Err(ConnectionLimitsError::MaxOutgoingConnectionsExceeded.into());
            }

            return Ok(max_outgoing_connections - self.outgoing_connections.len());
        }

        Ok(usize::MAX)
    }

    fn check_inbound(
        &mut self,
        _connection_id: ConnectionId,
        _address: SocketAddr,
    ) -> crate::Result<()> {
        if let Some(max_incoming_connections) = self.config.max_incoming_connections {
            if self.incoming_connections.len() >= max_incoming_connections {
                return Err(ConnectionLimitsError::MaxIncomingConnectionsExceeded.into());
            }
        }

        Ok(())
    }

    fn can_accept_connection(&mut self, _peer: PeerId, endpoint: &Endpoint) -> crate::Result<()> {
        // Check connection limits.
        if endpoint.is_listener() {
            if let Some(max_incoming_connections) = self.config.max_incoming_connections {
                if self.incoming_connections.len() >= max_incoming_connections {
                    return Err(ConnectionLimitsError::MaxIncomingConnectionsExceeded.into());
                }
            }
        } else if let Some(max_outgoing_connections) = self.config.max_outgoing_connections {
            if self.outgoing_connections.len() >= max_outgoing_connections {
                return Err(ConnectionLimitsError::MaxOutgoingConnectionsExceeded.into());
            }
        }

        Ok(())
    }

    fn on_connection_established(&mut self, _peer: PeerId, endpoint: &Endpoint) {
        if endpoint.is_listener() {
            if self.config.max_incoming_connections.is_some() {
                self.incoming_connections.insert(endpoint.connection_id());
            }
        } else if self.config.max_outgoing_connections.is_some() {
            self.outgoing_connections.insert(endpoint.connection_id());
        }
    }

    fn on_connection_closed(&mut self, _peer: PeerId, connection_id: ConnectionId) {
        self.incoming_connections.remove(&connection_id);
        self.outgoing_connections.remove(&connection_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConnectionId;

    #[test]
    fn connection_limits() {
        let config = ConnectionLimitsConfig::default()
            .max_incoming_connections(Some(3))
            .max_outgoing_connections(Some(2));
        let mut limits = ConnectionLimits::new(config);

        let connection_id_in_1 = ConnectionId::random();
        let connection_id_in_2 = ConnectionId::random();
        let connection_id_out_1 = ConnectionId::random();
        let connection_id_out_2 = ConnectionId::random();
        let connection_id_in_3 = ConnectionId::random();

        // Establish incoming connection.
        let endpoint = Endpoint::Listener {
            address: multiaddr::Multiaddr::empty(),
            connection_id: connection_id_in_1,
        };
        assert!(limits.can_accept_connection(PeerId::random(), &endpoint).is_ok());
        limits.on_connection_established(PeerId::random(), &endpoint);
        assert_eq!(limits.incoming_connections.len(), 1);

        let endpoint = Endpoint::Listener {
            address: multiaddr::Multiaddr::empty(),
            connection_id: connection_id_in_2,
        };
        assert!(limits.can_accept_connection(PeerId::random(), &endpoint).is_ok());
        limits.on_connection_established(PeerId::random(), &endpoint);
        assert_eq!(limits.incoming_connections.len(), 2);

        let endpoint = Endpoint::Listener {
            address: multiaddr::Multiaddr::empty(),
            connection_id: connection_id_in_3,
        };
        assert!(limits.can_accept_connection(PeerId::random(), &endpoint).is_ok());
        limits.on_connection_established(PeerId::random(), &endpoint);
        assert_eq!(limits.incoming_connections.len(), 3);

        assert!(limits.can_accept_connection(PeerId::random(), &endpoint).is_err());
        assert_eq!(limits.incoming_connections.len(), 3);

        // Establish outgoing connection.
        let endpoint = Endpoint::Dialer {
            address: multiaddr::Multiaddr::empty(),
            connection_id: connection_id_out_1,
        };
        assert!(limits.can_accept_connection(PeerId::random(), &endpoint).is_ok());
        limits.on_connection_established(PeerId::random(), &endpoint);
        assert_eq!(limits.incoming_connections.len(), 3);
        assert_eq!(limits.outgoing_connections.len(), 1);

        let endpoint = Endpoint::Dialer {
            address: multiaddr::Multiaddr::empty(),
            connection_id: connection_id_out_2,
        };
        assert!(limits.can_accept_connection(PeerId::random(), &endpoint).is_ok());
        limits.on_connection_established(PeerId::random(), &endpoint);
        assert_eq!(limits.incoming_connections.len(), 3);
        assert_eq!(limits.outgoing_connections.len(), 2);
        assert!(limits.can_accept_connection(PeerId::random(), &endpoint).is_err());

        // Close connections with 1.
        limits.on_connection_closed(PeerId::random(), connection_id_in_1);
        assert_eq!(limits.incoming_connections.len(), 2);
        assert_eq!(limits.outgoing_connections.len(), 2);

        limits.on_connection_closed(PeerId::random(), connection_id_out_1);
        assert_eq!(limits.incoming_connections.len(), 2);
        assert_eq!(limits.outgoing_connections.len(), 1);
    }
}
