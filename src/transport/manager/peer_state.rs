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

//! Peer state management.

use crate::{
    transport::{manager::SupportedTransport, Endpoint},
    types::ConnectionId,
    PeerId,
};

use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;

use std::collections::HashSet;

/// The peer state that tracks connections and dialing attempts.
///
/// # State Machine
///
/// ## [`PeerState::Disconnected`]
///
/// Initially, the peer is in the [`PeerState::Disconnected`] state without a
/// [`PeerState::Disconnected::dial_record`]. This means the peer is fully disconnected.
///
/// Next states:
/// - [`PeerState::Disconnected`] -> [`PeerState::Dialing`] (via [`PeerState::dial_single_address`])
/// - [`PeerState::Disconnected`] -> [`PeerState::Opening`] (via [`PeerState::dial_addresses`])
///
/// ## [`PeerState::Dialing`]
///
/// The peer can transition to the [`PeerState::Dialing`] state when a dialing attempt is
/// initiated. This only happens when the peer is dialed on a single address via
/// [`PeerState::dial_single_address`].
///
/// The dialing state implies the peer is reached on the socket address provided, as well as
/// negotiating noise and yamux protocols.
///
/// Next states:
/// - [`PeerState::Dialing`] -> [`PeerState::Connected`] (via
///   [`PeerState::on_connection_established`])
/// - [`PeerState::Dialing`] -> [`PeerState::Disconnected`] (via [`PeerState::on_dial_failure`])
///
/// ## [`PeerState::Opening`]
///
/// The peer can transition to the [`PeerState::Opening`] state when a dialing attempt is
/// initiated on multiple addresses via [`PeerState::dial_addresses`]. This takes into account
/// the parallelism factor (8 maximum) of the dialing attempts.
///
/// The opening state holds information about which protocol is being dialed to properly report back
/// errors.
///
/// The opening state is similar to the dial state, however the peer is only reached on a socket
/// address. The noise and yamux protocols are not negotiated yet. This state transitions to
/// [`PeerState::Dialing`] for the final part of the negotiation. Please note that it would be
/// wasteful to negotiate the noise and yamux protocols on all addresses, since only one
/// connection is kept around.
///
/// This is something we'll reconsider in the future if we encounter issues.
///
/// Next states:
/// - [`PeerState::Opening`] -> [`PeerState::Dialing`] (via transport manager
///   `on_connection_opened`)
/// - [`PeerState::Opening`] -> [`PeerState::Disconnected`] (via transport manager
///   `on_connection_opened` if negotiation cannot be started or via `on_open_failure`)
#[derive(Debug, Clone, PartialEq)]
pub enum PeerState {
    /// `Litep2p` is connected to peer.
    Connected {
        /// The established record of the connection.
        record: ConnectionRecord,

        /// Secondary record, this can either be a dial record or an established connection.
        ///
        /// While the local node was dialing a remote peer, the remote peer might've dialed
        /// the local node and connection was established successfully. This dial address
        /// is stored for processing later when the dial attempt concluded as either
        /// successful/failed.
        secondary: Option<SecondaryOrDialing>,
    },

    /// Connection to peer is opening over one or more addresses.
    Opening {
        /// Address records used for dialing.
        addresses: HashSet<Multiaddr>,

        /// Connection ID.
        connection_id: ConnectionId,

        /// Active transports.
        transports: HashSet<SupportedTransport>,
    },

    /// Peer is being dialed.
    Dialing {
        /// Address record.
        dial_record: ConnectionRecord,
    },

    /// `Litep2p` is not connected to peer.
    Disconnected {
        /// Dial address, if it exists.
        ///
        /// While the local node was dialing a remote peer, the remote peer might've dialed
        /// the local node and connection was established successfully. The connection might've
        /// been closed before the dial concluded which means that
        /// [`crate::transport::manager::TransportManager`] must be prepared to handle the dial
        /// failure even after the connection has been closed.
        dial_record: Option<ConnectionRecord>,
    },
}

/// The state of the secondary connection.
#[derive(Debug, Clone, PartialEq)]
pub enum SecondaryOrDialing {
    /// The secondary connection is established.
    Secondary(ConnectionRecord),
    /// The primary connection is established, but the secondary connection is still dialing.
    Dialing(ConnectionRecord),
}

/// Result of initiating a dial.
#[derive(Debug, Clone, PartialEq)]
pub enum StateDialResult {
    /// The peer is already connected.
    AlreadyConnected,
    /// The dialing state is already in progress.
    DialingInProgress,
    /// The peer is disconnected, start dialing.
    Ok,
}

impl PeerState {
    /// Check if the peer can be dialed.
    pub fn can_dial(&self) -> StateDialResult {
        match self {
            // The peer is already connected, no need to dial again.
            Self::Connected { .. } => return StateDialResult::AlreadyConnected,
            // The dialing state is already in progress, an event will be emitted later.
            Self::Dialing { .. }
            | Self::Opening { .. }
            | Self::Disconnected {
                dial_record: Some(_),
            } => {
                return StateDialResult::DialingInProgress;
            }

            Self::Disconnected { dial_record: None } => StateDialResult::Ok,
        }
    }

    /// Dial the peer on a single address.
    pub fn dial_single_address(&mut self, dial_record: ConnectionRecord) -> StateDialResult {
        let check = self.can_dial();
        if check != StateDialResult::Ok {
            return check;
        }

        match self {
            Self::Disconnected { dial_record: None } => {
                *self = PeerState::Dialing { dial_record };
                return StateDialResult::Ok;
            }
            state => panic!(
                "unexpected state: {:?} validated by Self::can_dial; qed",
                state
            ),
        }
    }

    /// Dial the peer on multiple addresses.
    pub fn dial_addresses(
        &mut self,
        connection_id: ConnectionId,
        addresses: HashSet<Multiaddr>,
        transports: HashSet<SupportedTransport>,
    ) -> StateDialResult {
        let check = self.can_dial();
        if check != StateDialResult::Ok {
            return check;
        }

        match self {
            Self::Disconnected { dial_record: None } => {
                *self = PeerState::Opening {
                    addresses,
                    connection_id,
                    transports,
                };
                return StateDialResult::Ok;
            }
            state => panic!(
                "unexpected state: {:?} validated by Self::can_dial; qed",
                state
            ),
        }
    }

    /// Handle dial failure.
    ///
    /// # Transitions
    /// - [`PeerState::Dialing`] (with record) -> [`PeerState::Disconnected`]
    /// - [`PeerState::Connected`] (with dial record) -> [`PeerState::Connected`]
    /// - [`PeerState::Disconnected`] (with dial record) -> [`PeerState::Disconnected`]
    pub fn on_dial_failure(&mut self, connection_id: ConnectionId) {
        match self {
            // Clear the dial record if the connection ID matches.
            Self::Dialing { dial_record } =>
                if dial_record.connection_id == connection_id {
                    *self = Self::Disconnected { dial_record: None };
                },

            Self::Connected {
                record,
                secondary: Some(SecondaryOrDialing::Dialing(dial_record)),
            } =>
                if dial_record.connection_id == connection_id {
                    *self = Self::Connected {
                        record: record.clone(),
                        secondary: None,
                    };
                },

            Self::Disconnected {
                dial_record: Some(dial_record),
            } =>
                if dial_record.connection_id == connection_id {
                    *self = Self::Disconnected { dial_record: None };
                },

            _ => (),
        };
    }

    /// Returns `true` if the connection should be accepted by the transport manager.
    pub fn on_connection_established(&mut self, connection: ConnectionRecord) -> bool {
        match self {
            // Transform the dial record into a secondary connection.
            Self::Connected {
                record,
                secondary: Some(SecondaryOrDialing::Dialing(dial_record)),
            } =>
                if dial_record.connection_id == connection.connection_id {
                    *self = Self::Connected {
                        record: record.clone(),
                        secondary: Some(SecondaryOrDialing::Secondary(connection)),
                    };

                    return true;
                },
            // There's place for a secondary connection.
            Self::Connected {
                record,
                secondary: None,
            } => {
                *self = Self::Connected {
                    record: record.clone(),
                    secondary: Some(SecondaryOrDialing::Secondary(connection)),
                };

                return true;
            }

            // Convert the dial record into a primary connection or preserve it.
            Self::Dialing { dial_record }
            | Self::Disconnected {
                dial_record: Some(dial_record),
            } =>
                if dial_record.connection_id == connection.connection_id {
                    *self = Self::Connected {
                        record: connection.clone(),
                        secondary: None,
                    };
                    return true;
                } else {
                    *self = Self::Connected {
                        record: connection,
                        secondary: Some(SecondaryOrDialing::Dialing(dial_record.clone())),
                    };
                    return true;
                },

            Self::Disconnected { dial_record: None } => {
                *self = Self::Connected {
                    record: connection,
                    secondary: None,
                };

                return true;
            }

            // Accept the incoming connection.
            Self::Opening { .. } => {
                *self = Self::Connected {
                    record: connection,
                    secondary: None,
                };

                return true;
            }

            _ => {}
        };

        return false;
    }

    /// Returns `true` if the connection was closed.
    pub fn on_connection_closed(&mut self, connection_id: ConnectionId) -> bool {
        match self {
            Self::Connected { record, secondary } => {
                // Primary connection closed.
                if record.connection_id == connection_id {
                    match secondary {
                        // Promote secondary connection to primary.
                        Some(SecondaryOrDialing::Secondary(secondary)) => {
                            *self = Self::Connected {
                                record: secondary.clone(),
                                secondary: None,
                            };
                        }
                        // Preserve the dial record.
                        Some(SecondaryOrDialing::Dialing(dial_record)) => {
                            *self = Self::Disconnected {
                                dial_record: Some(dial_record.clone()),
                            };

                            return true;
                        }
                        None => {
                            *self = Self::Disconnected { dial_record: None };

                            return true;
                        }
                    };

                    return false;
                }

                match secondary {
                    // Secondary connection closed.
                    Some(SecondaryOrDialing::Secondary(secondary))
                        if secondary.connection_id == connection_id =>
                    {
                        *self = Self::Connected {
                            record: record.clone(),
                            secondary: None,
                        };
                    }
                    _ => (),
                }
            }
            _ => (),
        }

        false
    }

    /// Returns `true` if the last transport failed to open.
    pub fn on_open_failure(&mut self, transport: SupportedTransport) -> bool {
        match self {
            Self::Opening { transports, .. } => {
                transports.remove(&transport);

                if transports.is_empty() {
                    *self = Self::Disconnected { dial_record: None };
                    return true;
                }

                return false;
            }
            _ => false,
        }
    }
}

/// The connection record keeps track of the connection ID and the address of the connection.
///
/// The connection ID is used to track the connection in the transport layer.
/// While the address is used to keep a healthy view of the network for dialing purposes.
///
/// # Note
///
/// The structure is used to keep track of:
///
///  - dialing state for outbound connections.
///  - established outbound connections via [`PeerState::Connected`].
///  - established inbound connections via `PeerContext::secondary_connection`.
#[derive(Debug, Clone, Hash, PartialEq)]
pub struct ConnectionRecord {
    /// Address of the connection.
    ///
    /// The address must contain the peer ID extension `/p2p/<peer_id>`.
    pub address: Multiaddr,

    /// Connection ID resulted from dialing.
    pub connection_id: ConnectionId,
}

impl ConnectionRecord {
    /// Construct a new connection record.
    pub fn new(peer: PeerId, address: Multiaddr, connection_id: ConnectionId) -> Self {
        Self {
            address: Self::ensure_peer_id(peer, address),
            connection_id,
        }
    }

    /// Create a new connection record from the peer ID and the endpoint.
    pub fn from_endpoint(peer: PeerId, endpoint: &Endpoint) -> Self {
        Self {
            address: Self::ensure_peer_id(peer, endpoint.address().clone()),
            connection_id: endpoint.connection_id(),
        }
    }

    /// Ensures the peer ID is present in the address.
    fn ensure_peer_id(peer: PeerId, address: Multiaddr) -> Multiaddr {
        if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
            address.with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).expect("valid peer id"),
            ))
        } else {
            address
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_can_dial() {
        let state = PeerState::Disconnected { dial_record: None };
        assert_eq!(state.can_dial(), StateDialResult::Ok);

        let record = ConnectionRecord::new(
            PeerId::random(),
            "/ip4/1.1.1.1/tcp/80".parse().unwrap(),
            ConnectionId::from(0),
        );

        let state = PeerState::Disconnected {
            dial_record: Some(record.clone()),
        };
        assert_eq!(state.can_dial(), StateDialResult::DialingInProgress);

        let state = PeerState::Dialing {
            dial_record: record.clone(),
        };
        assert_eq!(state.can_dial(), StateDialResult::DialingInProgress);

        let state = PeerState::Opening {
            addresses: Default::default(),
            connection_id: ConnectionId::from(0),
            transports: Default::default(),
        };
        assert_eq!(state.can_dial(), StateDialResult::DialingInProgress);

        let state = PeerState::Connected {
            record,
            secondary: None,
        };
        assert_eq!(state.can_dial(), StateDialResult::AlreadyConnected);
    }

    #[test]
    fn state_dial_single_address() {
        let record = ConnectionRecord::new(
            PeerId::random(),
            "/ip4/1.1.1.1/tcp/80".parse().unwrap(),
            ConnectionId::from(0),
        );

        let mut state = PeerState::Disconnected { dial_record: None };
        assert_eq!(
            state.dial_single_address(record.clone()),
            StateDialResult::Ok
        );
        assert_eq!(
            state,
            PeerState::Dialing {
                dial_record: record
            }
        );
    }

    #[test]
    fn state_dial_addresses() {
        let mut state = PeerState::Disconnected { dial_record: None };
        assert_eq!(
            state.dial_addresses(
                ConnectionId::from(0),
                Default::default(),
                Default::default()
            ),
            StateDialResult::Ok
        );
        assert_eq!(
            state,
            PeerState::Opening {
                addresses: Default::default(),
                connection_id: ConnectionId::from(0),
                transports: Default::default()
            }
        );
    }

    #[test]
    fn check_dial_failure() {
        let record = ConnectionRecord::new(
            PeerId::random(),
            "/ip4/1.1.1.1/tcp/80".parse().unwrap(),
            ConnectionId::from(0),
        );

        // Check from the dialing state.
        {
            let mut state = PeerState::Dialing {
                dial_record: record.clone(),
            };
            let previous_state = state.clone();
            // Check with different connection ID.
            state.on_dial_failure(ConnectionId::from(1));
            assert_eq!(state, previous_state);

            // Check with the same connection ID.
            state.on_dial_failure(ConnectionId::from(0));
            assert_eq!(state, PeerState::Disconnected { dial_record: None });
        }

        // Check from the connected state without dialing state.
        {
            let mut state = PeerState::Connected {
                record: record.clone(),
                secondary: None,
            };
            let previous_state = state.clone();
            // Check with different connection ID.
            state.on_dial_failure(ConnectionId::from(1));
            assert_eq!(state, previous_state);

            // Check with the same connection ID.
            // The connection ID is checked against dialing records, not established connections.
            state.on_dial_failure(ConnectionId::from(0));
            assert_eq!(state, previous_state);
        }

        // Check from the connected state with dialing state.
        {
            let mut state = PeerState::Connected {
                record: record.clone(),
                secondary: Some(SecondaryOrDialing::Dialing(record.clone())),
            };
            let previous_state = state.clone();
            // Check with different connection ID.
            state.on_dial_failure(ConnectionId::from(1));
            assert_eq!(state, previous_state);

            // Check with the same connection ID.
            // Dial record is cleared.
            state.on_dial_failure(ConnectionId::from(0));
            assert_eq!(
                state,
                PeerState::Connected {
                    record: record.clone(),
                    secondary: None,
                }
            );
        }

        // Check from the disconnected state.
        {
            let mut state = PeerState::Disconnected {
                dial_record: Some(record.clone()),
            };
            let previous_state = state.clone();
            // Check with different connection ID.
            state.on_dial_failure(ConnectionId::from(1));
            assert_eq!(state, previous_state);

            // Check with the same connection ID.
            state.on_dial_failure(ConnectionId::from(0));
            assert_eq!(state, PeerState::Disconnected { dial_record: None });
        }
    }
}
