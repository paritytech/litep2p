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

use crate::{codec::unsigned_varint::UnsignedVarint, peer_id::PeerId};

use bytes::BytesMut;
use multiaddr::Multiaddr;
use prost::Message;

/// Logging target for the file.
const LOG_TARGET: &str = "ifps::kademlia::message";

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

/// Kademlia message.
#[derive(Debug)]
pub(super) enum KademliaMessage {
    /// Found peer.
    FindNode {
        /// Found peers.
        peers: Vec<KademliaPeer>,
    },
}

mod schema {
    pub(super) mod kademlia {
        include!(concat!(env!("OUT_DIR"), "/kademlia.rs"));
    }
}

impl KademliaMessage {
    /// Create `FIND_NODE` message for `peer` and encode it using `UnsignedVarint`.
    pub(super) fn find_node(peer: PeerId) -> Vec<u8> {
        let message = schema::kademlia::Message {
            key: peer.to_bytes(),
            r#type: schema::kademlia::MessageType::FindNode.into(),
            cluster_level_raw: 10,
            ..Default::default()
        };

        let mut buf = Vec::with_capacity(message.encoded_len());
        message
            .encode(&mut buf)
            .expect("Vec<u8> to provide needed capacity");

        buf
    }

    /// Create `GET_VALUE` message.
    pub(super) fn get_value() -> Vec<u8> {
        todo!();
    }

    /// Create `PUT_VALUE` message.
    pub(super) fn put_value() -> Vec<u8> {
        todo!();
    }

    /// Get [`KademliaMessage`] from bytes.
    pub fn from_bytes(bytes: BytesMut) -> Option<Self> {
        match schema::kademlia::Message::decode(bytes) {
            Ok(message) => match message.r#type {
                4 => {
                    let peers = message
                        .closer_peers
                        .iter()
                        .filter_map(|peer| KademliaPeer::try_from(peer).ok())
                        .collect();

                    Some(Self::FindNode { peers })
                }
                _ => {
                    todo!("unsupported message type");
                }
            },
            Err(error) => {
                tracing::debug!(target: LOG_TARGET, ?error, "failed to decode message");
                None
            }
        }
    }
}
