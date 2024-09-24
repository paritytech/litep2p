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
