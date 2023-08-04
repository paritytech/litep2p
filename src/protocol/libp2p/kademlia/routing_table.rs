// Copyright 2018 Parity Technologies (UK) Ltd.
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

//! Kademlia routing table implementation.
//!
//! The routing table has a simple design:
//!  - calculate k-bucket for the peer from its peer ID
//!  - if the the k-bucket has space, insert peer into the k-bucket
//!  - if the the k-bucket is full, check if there are any disconnected peers
//!     - if
//!
//!
//!
//!

use crate::{
    peer_id::PeerId,
    protocol::libp2p::kademlia::{
        bucket::{KBucket, KBucketEntry},
        key::{Distance, Key, U256},
        types::KademliaPeer,
    },
};

use multiaddr::Multiaddr;

use std::{collections::VecDeque, time::Duration};

/// Number of k-buckets.
const NUM_BUCKETS: usize = 256;

pub struct RoutingTable {
    /// Local key.
    local_key: Key<PeerId>,

    /// K-buckets.
    buckets: Vec<KBucket>,
}

/// A (type-safe) index into a `KBucketsTable`, i.e. a non-negative integer in the
/// interval `[0, NUM_BUCKETS)`.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct BucketIndex(usize);

impl BucketIndex {
    /// Creates a new `BucketIndex` for a `Distance`.
    ///
    /// The given distance is interpreted as the distance from a `local_key` of
    /// a `KBucketsTable`. If the distance is zero, `None` is returned, in
    /// recognition of the fact that the only key with distance `0` to a
    /// `local_key` is the `local_key` itself, which does not belong in any
    /// bucket.
    fn new(d: &Distance) -> Option<BucketIndex> {
        d.ilog2().map(|i| BucketIndex(i as usize))
    }

    /// Gets the index value as an unsigned integer.
    fn get(&self) -> usize {
        self.0
    }

    /// Returns the minimum inclusive and maximum inclusive [`Distance`]
    /// included in the bucket for this index.
    fn range(&self) -> (Distance, Distance) {
        let min = Distance(U256::pow(U256::from(2), U256::from(self.0)));
        if self.0 == usize::from(u8::MAX) {
            (min, Distance(U256::MAX))
        } else {
            let max = Distance(U256::pow(U256::from(2), U256::from(self.0 + 1)) - 1);
            (min, max)
        }
    }

    /// Generates a random distance that falls into the bucket for this index.
    fn rand_distance(&self, rng: &mut impl rand::Rng) -> Distance {
        let mut bytes = [0u8; 32];
        let quot = self.0 / 8;
        for i in 0..quot {
            bytes[31 - i] = rng.gen();
        }
        let rem = (self.0 % 8) as u32;
        let lower = usize::pow(2, rem);
        let upper = usize::pow(2, rem + 1);
        bytes[31 - quot] = rng.gen_range(lower, upper) as u8;
        Distance(U256::from(bytes))
    }
}

impl RoutingTable {
    /// Create new [`RoutingTable`].
    pub fn new(local_key: Key<PeerId>) -> Self {
        RoutingTable {
            local_key,
            buckets: (0..NUM_BUCKETS).map(|_| KBucket::new()).collect(),
        }
    }

    /// Returns the local key.
    pub fn local_key(&self) -> &Key<PeerId> {
        &self.local_key
    }

    /// Get an entry for `peer` into a k-bucket.
    pub fn entry<'a>(&'a mut self, peer: PeerId) -> KBucketEntry<'a> {
        let key = Key::from(peer);

        let Some(index) = BucketIndex::new(&self.local_key.distance(&key)) else {
            return KBucketEntry::LocalNode
        };

        self.buckets[index.get()].entry(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::libp2p::kademlia::types::ConnectionType;

    #[test]
    fn add_peer_to_empty_table() {
        let own_peer_id = PeerId::random();
        let own_key = Key::from(own_peer_id);
        let mut table = RoutingTable::new(own_key.clone());

        // verify that local peer id resolves to special entry
        assert_eq!(table.entry(own_peer_id), KBucketEntry::LocalNode);

        let peer = PeerId::random();
        let mut test = table.entry(peer);
        let addresses = vec![];

        assert!(std::matches!(test, KBucketEntry::Vacant(_)));
        test.insert(KademliaPeer {
            peer,
            addresses: addresses.clone(),
            connection: ConnectionType::Connected,
        });

        assert!(std::matches!(
            table.entry(peer),
            KBucketEntry::Occupied(KademliaPeer {
                peer,
                addresses,
                connection: ConnectionType::Connected,
            })
        ));

        match table.entry(peer) {
            KBucketEntry::Occupied(entry) => {
                entry.connection = ConnectionType::NotConnected;
            }
            state => panic!("invalid state for `KBucketEntry`"),
        }

        assert!(std::matches!(
            table.entry(peer),
            KBucketEntry::Occupied(KademliaPeer {
                peer,
                addresses,
                connection: ConnectionType::NotConnected,
            })
        ));
    }
}
