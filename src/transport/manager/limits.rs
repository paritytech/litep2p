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

use crate::types::ConnectionId;

use std::{
    collections::HashSet,
    time::{Duration, Instant},
};
use tokio::sync::OwnedSemaphorePermit;

/// Default limit of concurrent incoming openings, per transport.
pub(crate) const DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS: usize = 256;

/// Configuration for the connection limits.
#[derive(Debug, Clone)]
pub struct ConnectionLimitsConfig {
    /// Maximum number of incoming connections being opened concurrently, per transport.
    max_pending_incoming_connections: usize,
    /// Maximum number of incoming connections that can be established.
    max_incoming_connections: Option<usize>,
    /// Maximum number of outgoing connections that can be established.
    max_outgoing_connections: Option<usize>,
}

impl Default for ConnectionLimitsConfig {
    fn default() -> Self {
        Self {
            max_pending_incoming_connections: DEFAULT_MAX_PENDING_INCOMING_CONNECTIONS,
            max_incoming_connections: None,
            max_outgoing_connections: None,
        }
    }
}

impl ConnectionLimitsConfig {
    /// Limit concurrent incoming openings. Defaults to 256.
    ///
    /// Applies separately to each transport, so one transport can't starve the others. Zero
    /// disables incoming connections.
    pub fn max_pending_incoming_connections(mut self, limit: usize) -> Self {
        self.max_pending_incoming_connections = limit;
        self
    }

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

/// Reserves opening capacity until acceptance or removal.
#[derive(Debug)]
pub(crate) struct InboundConnectionPermit {
    pub(crate) _permit: OwnedSemaphorePermit,
    pub(crate) deadline: Instant,
}

impl InboundConnectionPermit {
    pub(crate) fn is_expired(&self) -> bool {
        Instant::now() >= self.deadline
    }
}

/// Minimum interval between reports of dropped inbound openings.
const DROPPED_OPENINGS_REPORT_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Rate-limits reports of inbound openings dropped at capacity.
#[derive(Debug, Default)]
pub(crate) struct DroppedOpenings {
    /// Drops since the last report.
    count: usize,
    /// Time of the last report.
    last_report: Option<Instant>,
}

impl DroppedOpenings {
    /// Record a drop.
    ///
    /// Returns the number of drops since the last report if a report is due: on the first
    /// drop, then at most once per interval.
    pub(crate) fn record(&mut self) -> Option<usize> {
        self.record_at(Instant::now())
    }

    fn record_at(&mut self, now: Instant) -> Option<usize> {
        self.count += 1;

        if self.last_report.is_some_and(|last_report| {
            now.saturating_duration_since(last_report) < DROPPED_OPENINGS_REPORT_INTERVAL
        }) {
            return None;
        }

        self.last_report = Some(now);
        Some(std::mem::take(&mut self.count))
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

    /// Per-transport capacity for incoming openings.
    pub(super) fn max_pending_incoming_connections(&self) -> usize {
        self.config.max_pending_incoming_connections
    }

    /// Called when dialing an address.
    ///
    /// Returns the number of outgoing connections permitted to be established.
    /// It is guaranteed that at least one connection can be established if the method returns `Ok`.
    /// The number of available outgoing connections can influence the maximum parallel dials to a
    /// single address.
    ///
    /// If the maximum number of outgoing connections is not set, `Ok(usize::MAX)` is returned.
    pub fn on_dial_address(&mut self) -> Result<usize, ConnectionLimitsError> {
        if let Some(max_outgoing_connections) = self.config.max_outgoing_connections {
            if self.outgoing_connections.len() >= max_outgoing_connections {
                return Err(ConnectionLimitsError::MaxOutgoingConnectionsExceeded);
            }

            return Ok(max_outgoing_connections - self.outgoing_connections.len());
        }

        Ok(usize::MAX)
    }

    /// Called before accepting a new incoming connection.
    pub fn on_incoming(&mut self) -> Result<(), ConnectionLimitsError> {
        if let Some(max_incoming_connections) = self.config.max_incoming_connections {
            if self.incoming_connections.len() >= max_incoming_connections {
                return Err(ConnectionLimitsError::MaxIncomingConnectionsExceeded);
            }
        }

        Ok(())
    }

    /// Called when a new connection is established.
    ///
    /// Returns an error if the connection cannot be accepted due to connection limits.
    pub fn can_accept_connection(
        &mut self,
        is_listener: bool,
    ) -> Result<(), ConnectionLimitsError> {
        // Check connection limits.
        if is_listener {
            if let Some(max_incoming_connections) = self.config.max_incoming_connections {
                if self.incoming_connections.len() >= max_incoming_connections {
                    return Err(ConnectionLimitsError::MaxIncomingConnectionsExceeded);
                }
            }
        } else if let Some(max_outgoing_connections) = self.config.max_outgoing_connections {
            if self.outgoing_connections.len() >= max_outgoing_connections {
                return Err(ConnectionLimitsError::MaxOutgoingConnectionsExceeded);
            }
        }

        Ok(())
    }

    /// Accept an established connection.
    ///
    /// # Note
    ///
    /// This method should be called after the `Self::can_accept_connection` method
    /// to ensure that the connection can be accepted.
    pub fn accept_established_connection(
        &mut self,
        connection_id: ConnectionId,
        is_listener: bool,
    ) {
        if is_listener {
            if self.config.max_incoming_connections.is_some() {
                self.incoming_connections.insert(connection_id);
            }
        } else if self.config.max_outgoing_connections.is_some() {
            self.outgoing_connections.insert(connection_id);
        }
    }

    /// Called when a connection is closed.
    pub fn on_connection_closed(&mut self, connection_id: ConnectionId) {
        self.incoming_connections.remove(&connection_id);
        self.outgoing_connections.remove(&connection_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ConnectionId;

    #[test]
    fn dropped_openings_reports_are_rate_limited() {
        let mut dropped = DroppedOpenings::default();
        let start = Instant::now();

        assert_eq!(dropped.record_at(start), Some(1));
        assert_eq!(dropped.record_at(start), None);
        assert_eq!(
            dropped.record_at(start + DROPPED_OPENINGS_REPORT_INTERVAL / 2),
            None
        );
        assert_eq!(
            dropped.record_at(start + DROPPED_OPENINGS_REPORT_INTERVAL),
            Some(3)
        );
        assert_eq!(
            dropped.record_at(start + DROPPED_OPENINGS_REPORT_INTERVAL),
            None
        );
    }

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
        assert!(limits.can_accept_connection(true).is_ok());
        limits.accept_established_connection(connection_id_in_1, true);
        assert_eq!(limits.incoming_connections.len(), 1);

        assert!(limits.can_accept_connection(true).is_ok());
        limits.accept_established_connection(connection_id_in_2, true);
        assert_eq!(limits.incoming_connections.len(), 2);

        assert!(limits.can_accept_connection(true).is_ok());
        limits.accept_established_connection(connection_id_in_3, true);
        assert_eq!(limits.incoming_connections.len(), 3);

        assert_eq!(
            limits.can_accept_connection(true).unwrap_err(),
            ConnectionLimitsError::MaxIncomingConnectionsExceeded
        );
        assert_eq!(limits.incoming_connections.len(), 3);

        // Establish outgoing connection.
        assert!(limits.can_accept_connection(false).is_ok());
        limits.accept_established_connection(connection_id_out_1, false);
        assert_eq!(limits.incoming_connections.len(), 3);
        assert_eq!(limits.outgoing_connections.len(), 1);

        assert!(limits.can_accept_connection(false).is_ok());
        limits.accept_established_connection(connection_id_out_2, false);
        assert_eq!(limits.incoming_connections.len(), 3);
        assert_eq!(limits.outgoing_connections.len(), 2);

        assert_eq!(
            limits.can_accept_connection(false).unwrap_err(),
            ConnectionLimitsError::MaxOutgoingConnectionsExceeded
        );

        // Close connections with peer a.
        limits.on_connection_closed(connection_id_in_1);
        assert_eq!(limits.incoming_connections.len(), 2);
        assert_eq!(limits.outgoing_connections.len(), 2);

        limits.on_connection_closed(connection_id_out_1);
        assert_eq!(limits.incoming_connections.len(), 2);
        assert_eq!(limits.outgoing_connections.len(), 1);
    }
}
