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

//! Kademlia types.

use crate::{peer_id::PeerId, protocol::libp2p::kademlia::schema};

use multiaddr::Multiaddr;

/// Connection type to peer.
#[derive(Debug)]
pub enum ConnectionType {
    /// Sender does not have a connection to peer.
    NotConnected,

    /// Sender is connected to the peer.
    Connected,

    /// Sender has recently been connected to the peer.
    CanConnect,

    /// Sender is unable to connect to the peer.
    CannotConnect,
}

impl TryFrom<i32> for ConnectionType {
    type Error = ();

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(ConnectionType::NotConnected),
            1 => Ok(ConnectionType::Connected),
            2 => Ok(ConnectionType::CanConnect),
            3 => Ok(ConnectionType::CannotConnect),
            _ => Err(()),
        }
    }
}

/// Kademlia peer.
#[derive(Debug)]
pub struct KademliaPeer {
    /// Peer ID.
    pub(super) peer: PeerId,

    /// Known addresses of peer.
    pub(super) addresses: Vec<Multiaddr>,

    /// Connection type.
    pub(super) connection: ConnectionType,
}

impl TryFrom<&schema::kademlia::Peer> for KademliaPeer {
    type Error = ();

    fn try_from(record: &schema::kademlia::Peer) -> Result<Self, Self::Error> {
        Ok(KademliaPeer {
            peer: PeerId::from_bytes(&record.id).map_err(|_| ())?,
            addresses: record
                .addrs
                .iter()
                .filter_map(|address| Multiaddr::try_from(address.clone()).ok())
                .collect(),
            connection: ConnectionType::try_from(record.connection)?,
        })
    }
}
