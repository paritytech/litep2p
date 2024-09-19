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
    transport::{manager::address::AddressStore, Endpoint},
    types::ConnectionId,
    Error, PeerId,
};

use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;

use std::collections::HashSet;

/// Supported protocols.
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum SupportedTransport {
    /// TCP.
    Tcp,

    /// QUIC.
    #[cfg(feature = "quic")]
    Quic,

    /// WebRTC
    #[cfg(feature = "webrtc")]
    WebRtc,

    /// WebSocket
    #[cfg(feature = "websocket")]
    WebSocket,
}

/// Peer state.
#[derive(Debug)]
pub enum PeerState {
    /// `Litep2p` is connected to peer.
    Connected {
        /// The established record of the connection.
        record: ConnectionRecord,

        /// Dial address, if it exists.
        ///
        /// While the local node was dialing a remote peer, the remote peer might've dialed
        /// the local node and connection was established successfully. This dial address
        /// is stored for processing later when the dial attempt concluded as either
        /// successful/failed.
        dial_record: Option<ConnectionRecord>,
    },

    /// Connection to peer is opening over one or more addresses.
    Opening {
        /// Address records used for dialing.
        records: HashSet<Multiaddr>,

        /// Connection ID.
        connection_id: ConnectionId,

        /// Active transports.
        transports: HashSet<SupportedTransport>,
    },

    /// Peer is being dialed.
    Dialing {
        /// Address record.
        record: ConnectionRecord,
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

impl PeerState {
    /// Advances the peer state on a dial attempt.
    /// The dialing is happing on a single address.
    ///
    /// Provides a response if the dialing should return immediately.
    ///
    /// # Transitions
    ///
    /// [`PeerState::Disconnected`] -> [`PeerState::Dialing`]
    pub fn on_dial_record(&mut self, dial_record: ConnectionRecord) -> Option<Result<(), Error>> {
        match self {
            // The peer is already connected, no need to dial a second time.
            Self::Connected { .. } => {
                return Some(Err(Error::AlreadyConnected));
            }
            // The dialing state is already in progress, an event will be emitted later.
            Self::Dialing { .. }
            | Self::Opening { .. }
            | Self::Disconnected {
                dial_record: Some(_),
            } => {
                return Some(Ok(()));
            }
            // The peer is disconnected, start dialing.
            Self::Disconnected { dial_record: None } => {
                *self = Self::Dialing {
                    record: dial_record,
                };
                return None;
            }
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
#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Debug, Clone, Hash)]
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

/// Peer context.
#[derive(Debug)]
pub struct PeerContext {
    /// Peer state.
    pub state: PeerState,

    /// Secondary connection, if it's open.
    pub secondary_connection: Option<ConnectionRecord>,

    /// Known addresses of peer.
    pub addresses: AddressStore,
}

impl Default for PeerContext {
    fn default() -> Self {
        Self {
            state: PeerState::Disconnected { dial_record: None },
            secondary_connection: None,
            addresses: AddressStore::new(),
        }
    }
}
