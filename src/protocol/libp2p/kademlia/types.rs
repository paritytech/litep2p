// Copyright 2018-2019 Parity Technologies (UK) Ltd.
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

use crate::{protocol::libp2p::kademlia::schema, PeerId};

use multiaddr::Multiaddr;
use sha2::{
    digest::generic_array::{typenum::U32, GenericArray},
    Digest, Sha256,
};
use uint::*;

use std::{
    borrow::Borrow,
    hash::{Hash, Hasher},
};

construct_uint! {
    /// 256-bit unsigned integer.
    pub(super) struct U256(4);
}

/// A `Key` in the DHT keyspace with preserved preimage.
///
/// Keys in the DHT keyspace identify both the participating nodes, as well as
/// the records stored in the DHT.
///
/// `Key`s have an XOR metric as defined in the Kademlia paper, i.e. the bitwise XOR of
/// the hash digests, interpreted as an integer. See [`Key::distance`].
#[derive(Clone, Debug)]
pub struct Key<T: Clone> {
    preimage: T,
    bytes: KeyBytes,
}

impl<T: Clone> Key<T> {
    /// Constructs a new `Key` by running the given value through a random
    /// oracle.
    ///
    /// The preimage of type `T` is preserved.
    /// See [`Key::into_preimage`] for more details.
    pub fn new(preimage: T) -> Key<T>
    where
        T: Borrow<[u8]>,
    {
        let bytes = KeyBytes::new(preimage.borrow());
        Key { preimage, bytes }
    }

    /// Convert [`Key`] into its preimage.
    pub fn into_preimage(self) -> T {
        self.preimage
    }

    /// Computes the distance of the keys according to the XOR metric.
    pub fn distance<U>(&self, other: &U) -> Distance
    where
        U: AsRef<KeyBytes>,
    {
        self.bytes.distance(other)
    }

    /// Returns the uniquely determined key with the given distance to `self`.
    ///
    /// This implements the following equivalence:
    ///
    /// `self xor other = distance <==> other = self xor distance`
    #[cfg(test)]
    pub fn for_distance(&self, d: Distance) -> KeyBytes {
        self.bytes.for_distance(d)
    }

    /// Generate key from `KeyBytes` with a random preimage.
    ///
    /// Only used for testing
    #[cfg(test)]
    pub fn from_bytes(bytes: KeyBytes, preimage: T) -> Key<T> {
        Self { bytes, preimage }
    }
}

impl<T: Clone> From<Key<T>> for KeyBytes {
    fn from(key: Key<T>) -> KeyBytes {
        key.bytes
    }
}

impl From<PeerId> for Key<PeerId> {
    fn from(p: PeerId) -> Self {
        let bytes = KeyBytes(Sha256::digest(p.to_bytes()));
        Key { preimage: p, bytes }
    }
}

impl From<Vec<u8>> for Key<Vec<u8>> {
    fn from(b: Vec<u8>) -> Self {
        Key::new(b)
    }
}

impl<T: Clone> AsRef<KeyBytes> for Key<T> {
    fn as_ref(&self) -> &KeyBytes {
        &self.bytes
    }
}

impl<T: Clone, U: Clone> PartialEq<Key<U>> for Key<T> {
    fn eq(&self, other: &Key<U>) -> bool {
        self.bytes == other.bytes
    }
}

impl<T: Clone> Eq for Key<T> {}

impl<T: Clone> Hash for Key<T> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.bytes.0.hash(state);
    }
}

/// The raw bytes of a key in the DHT keyspace.
#[derive(PartialEq, Eq, Clone, Debug)]
pub struct KeyBytes(GenericArray<u8, U32>);

impl KeyBytes {
    /// Creates a new key in the DHT keyspace by running the given
    /// value through a random oracle.
    pub fn new<T>(value: T) -> Self
    where
        T: Borrow<[u8]>,
    {
        KeyBytes(Sha256::digest(value.borrow()))
    }

    /// Computes the distance of the keys according to the XOR metric.
    pub fn distance<U>(&self, other: &U) -> Distance
    where
        U: AsRef<KeyBytes>,
    {
        let a = U256::from(self.0.as_slice());
        let b = U256::from(other.as_ref().0.as_slice());
        Distance(a ^ b)
    }

    /// Returns the uniquely determined key with the given distance to `self`.
    ///
    /// This implements the following equivalence:
    ///
    /// `self xor other = distance <==> other = self xor distance`
    #[cfg(test)]
    pub fn for_distance(&self, d: Distance) -> KeyBytes {
        let key_int = U256::from(self.0.as_slice()) ^ d.0;
        KeyBytes(GenericArray::from(<[u8; 32]>::from(key_int)))
    }
}

impl AsRef<KeyBytes> for KeyBytes {
    fn as_ref(&self) -> &KeyBytes {
        self
    }
}

/// A distance between two keys in the DHT keyspace.
#[derive(Copy, Clone, PartialEq, Eq, Default, PartialOrd, Ord, Debug)]
pub struct Distance(pub(super) U256);

impl Distance {
    /// Returns the integer part of the base 2 logarithm of the [`Distance`].
    ///
    /// Returns `None` if the distance is zero.
    pub fn ilog2(&self) -> Option<u32> {
        (256 - self.0.leading_zeros()).checked_sub(1)
    }
}

/// Connection type to peer.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
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

impl From<ConnectionType> for i32 {
    fn from(connection: ConnectionType) -> Self {
        match connection {
            ConnectionType::NotConnected => 0,
            ConnectionType::Connected => 1,
            ConnectionType::CanConnect => 2,
            ConnectionType::CannotConnect => 3,
        }
    }
}

/// Kademlia peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KademliaPeer {
    /// Peer key.
    pub(super) key: Key<PeerId>,

    /// Peer ID.
    pub(super) peer: PeerId,

    /// Known addresses of peer.
    pub(super) addresses: Vec<Multiaddr>,

    /// Connection type.
    pub(super) connection: ConnectionType,
}

impl KademliaPeer {
    /// Create new [`KademliaPeer`].
    pub fn new(peer: PeerId, addresses: Vec<Multiaddr>, connection: ConnectionType) -> Self {
        Self {
            peer,
            addresses,
            connection,
            key: Key::from(peer),
        }
    }
}

impl TryFrom<&schema::kademlia::Peer> for KademliaPeer {
    type Error = ();

    fn try_from(record: &schema::kademlia::Peer) -> Result<Self, Self::Error> {
        let peer = PeerId::from_bytes(&record.id).map_err(|_| ())?;

        Ok(KademliaPeer {
            key: Key::from(peer),
            peer,
            addresses: record
                .addrs
                .iter()
                .filter_map(|address| Multiaddr::try_from(address.clone()).ok())
                .collect(),
            connection: ConnectionType::try_from(record.connection)?,
        })
    }
}

impl From<&KademliaPeer> for schema::kademlia::Peer {
    fn from(peer: &KademliaPeer) -> Self {
        schema::kademlia::Peer {
            id: peer.peer.to_bytes(),
            addrs: peer.addresses.iter().map(|address| address.to_vec()).collect(),
            connection: peer.connection.into(),
        }
    }
}
