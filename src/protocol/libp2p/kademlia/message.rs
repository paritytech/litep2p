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

use crate::protocol::libp2p::kademlia::{
    record::{Key as RecordKey, Record},
    schema,
    types::KademliaPeer,
};

use bytes::{Bytes, BytesMut};
use prost::Message;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::message";

/// Kademlia message.
#[derive(Debug, Clone)]
pub enum KademliaMessage {
    /// `FIND_NODE` message.
    FindNode {
        /// Query target.
        target: Vec<u8>,

        /// Found peers.
        peers: Vec<KademliaPeer>,
    },

    /// Kademlia `PUT_VALUE` message.
    PutValue {
        /// Record.
        record: Record,
    },

    /// `GET_VALUE` message.
    GetRecord {
        /// Key.
        key: Option<RecordKey>,

        /// Record.
        record: Option<Record>,

        /// Peers closest to key.
        peers: Vec<KademliaPeer>,
    },
}

impl KademliaMessage {
    /// Create `FIND_NODE` message for `peer`.
    pub fn find_node<T: Into<Vec<u8>>>(key: T) -> Bytes {
        let message = schema::kademlia::Message {
            key: key.into(),
            r#type: schema::kademlia::MessageType::FindNode.into(),
            cluster_level_raw: 10,
            ..Default::default()
        };

        let mut buf = BytesMut::with_capacity(message.encoded_len());
        message.encode(&mut buf).expect("Vec<u8> to provide needed capacity");

        buf.freeze()
    }

    /// Create `PUT_VALUE` message for `record`.
    // TODO: set ttl
    pub fn put_value(record: Record) -> Bytes {
        let message = schema::kademlia::Message {
            key: record.key.clone().into(),
            r#type: schema::kademlia::MessageType::PutValue.into(),
            record: Some(schema::kademlia::Record {
                key: record.key.into(),
                value: record.value,
                ..Default::default()
            }),
            cluster_level_raw: 10,
            ..Default::default()
        };

        let mut buf = BytesMut::with_capacity(message.encoded_len());
        message.encode(&mut buf).expect("BytesMut to provide needed capacity");

        buf.freeze()
    }

    /// Create `GET_VALUE` message for `record`.
    pub fn get_record(key: RecordKey) -> Bytes {
        let message = schema::kademlia::Message {
            key: key.clone().into(),
            r#type: schema::kademlia::MessageType::GetValue.into(),
            cluster_level_raw: 10,
            ..Default::default()
        };

        let mut buf = BytesMut::with_capacity(message.encoded_len());
        message.encode(&mut buf).expect("BytesMut to provide needed capacity");

        buf.freeze()
    }

    /// Create `FIND_NODE` response.
    pub fn find_node_response<K: AsRef<[u8]>>(key: K, peers: Vec<KademliaPeer>) -> Vec<u8> {
        let message = schema::kademlia::Message {
            key: key.as_ref().to_vec(),
            cluster_level_raw: 10,
            r#type: schema::kademlia::MessageType::FindNode.into(),
            closer_peers: peers.iter().map(|peer| peer.into()).collect(),
            ..Default::default()
        };

        let mut buf = Vec::with_capacity(message.encoded_len());
        message.encode(&mut buf).expect("Vec<u8> to provide needed capacity");

        buf
    }

    /// Create `PUT_VALUE` response.
    pub fn get_value_response(
        key: RecordKey,
        peers: Vec<KademliaPeer>,
        record: Option<Record>,
    ) -> Vec<u8> {
        let message = schema::kademlia::Message {
            key: key.to_vec(),
            cluster_level_raw: 10,
            r#type: schema::kademlia::MessageType::GetValue.into(),
            closer_peers: peers.iter().map(|peer| peer.into()).collect(),
            record: record.map(|record| schema::kademlia::Record {
                key: record.key.to_vec(),
                value: record.value,
                ..Default::default()
            }),
            ..Default::default()
        };

        let mut buf = Vec::with_capacity(message.encoded_len());
        message.encode(&mut buf).expect("Vec<u8> to provide needed capacity");

        buf
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

                    Some(Self::FindNode {
                        target: message.key,
                        peers,
                    })
                }
                0 => {
                    let record = message.record?;

                    Some(Self::PutValue {
                        record: Record::new(record.key, record.value),
                    })
                }
                1 => {
                    let key = match message.key.is_empty() {
                        true => message
                            .record
                            .as_ref()
                            .map(|record| {
                                (!record.key.is_empty())
                                    .then_some(RecordKey::from(record.key.clone()))
                            })
                            .flatten(),
                        false => Some(RecordKey::from(message.key.clone())),
                    };

                    Some(Self::GetRecord {
                        key,
                        record: message.record.map(|record| Record::new(record.key, record.value)),
                        peers: message
                            .closer_peers
                            .iter()
                            .filter_map(|peer| KademliaPeer::try_from(peer).ok())
                            .collect(),
                    })
                }
                message => {
                    tracing::warn!(target: LOG_TARGET, ?message, "unhandled message");
                    None
                }
            },
            Err(error) => {
                tracing::debug!(target: LOG_TARGET, ?error, "failed to decode message");
                None
            }
        }
    }
}
