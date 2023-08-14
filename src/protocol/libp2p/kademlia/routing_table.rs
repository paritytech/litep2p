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

use crate::{
    peer_id::PeerId,
    protocol::libp2p::kademlia::{
        bucket::{KBucket, KBucketEntry},
        types::{ConnectionType, Distance, KademliaPeer, Key, U256},
    },
};

use multiaddr::Multiaddr;

use std::{
    collections::{BTreeMap, VecDeque},
    time::Duration,
};

/// Number of k-buckets.
const NUM_BUCKETS: usize = 256;

/// Logging target for the file.
const LOG_TARGET: &str = "ipfs::kademlia::routing_table";

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
        // bytes[31 - quot] = rng.gen_range(lower, upper) as u8;
        bytes[31 - quot] = rng.gen_range(lower..upper) as u8;
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
    pub fn entry<'a>(&'a mut self, key: Key<PeerId>) -> KBucketEntry<'a> {
        let Some(index) = BucketIndex::new(&self.local_key.distance(&key)) else {
            return KBucketEntry::LocalNode;
        };

        self.buckets[index.get()].entry(key)
    }

    /// Add known peer to [`RoutingTable`].
    ///
    /// In order to bootstrap the lookup process, the routing table must be aware of at least one
    /// node and of its addresses. The insert operation is ignored
    pub fn add_known_peer(
        &mut self,
        peer: PeerId,
        addresses: Vec<Multiaddr>,
        connection: ConnectionType,
    ) {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?addresses,
            ?connection,
            "add known peer"
        );

        match (self.entry(Key::from(peer)), addresses.is_empty()) {
            (KBucketEntry::Occupied(entry), false) => {
                entry.addresses = addresses;
            }
            (mut entry @ KBucketEntry::Vacant(_), false) => {
                entry.insert(KademliaPeer::new(peer, addresses, connection));
            }
            (KBucketEntry::LocalNode, _) => tracing::warn!(
                target: LOG_TARGET,
                ?peer,
                "tried to add local node to routing table",
            ),
            (KBucketEntry::NoSlot, _) => tracing::trace!(
                target: LOG_TARGET,
                ?peer,
                "routing table full, cannot add new entry",
            ),
            (_, true) => tracing::debug!(
                target: LOG_TARGET,
                ?peer,
                "tried to add zero addresses to the routing table",
            ),
        }
    }

    /// Get `limit` closests peers to `target` from the k-buckets.
    pub fn closest<K: Clone>(&mut self, target: Key<K>, limit: usize) -> Vec<KademliaPeer> {
        let (index, mut peers): (_, Vec<_>) =
            match BucketIndex::new(&self.local_key.distance(&target)) {
                Some(index) => (
                    index.get(),
                    self.buckets[index.get()]
                        .closest_iter(target.clone())
                        .cloned()
                        .collect(),
                ),
                None => (
                    0,
                    self.buckets[0]
                        .closest_iter(target.clone())
                        .cloned()
                        .collect(),
                ),
            };

        // TODO: this is hideous, use a more intelligent approach
        if peers.len() < limit {
            let mut nodes = Vec::new();

            for (i, bucket) in self.buckets.iter_mut().enumerate() {
                if i == index {
                    continue;
                }

                nodes.extend(bucket.closest_iter(target.clone()));
            }

            nodes.sort_by(|a, b| target.distance(&a.key).cmp(&target.distance(&b.key)));

            let count = {
                if limit > nodes.len() {
                    nodes.len()
                } else {
                    (limit - peers.len())
                }
            };

            for i in 0..count {
                peers.push(nodes[i].clone());
            }
        }

        peers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::libp2p::kademlia::types::ConnectionType;
    use rand::Rng;
    use sha2::digest::generic_array::{typenum::U32, GenericArray};

    #[test]
    fn closest_peers() {
        let mut rng = rand::thread_rng();
        let own_peer_id = PeerId::random();
        let own_key = Key::from(own_peer_id);
        let mut table = RoutingTable::new(own_key.clone());

        for i in 0..60 {
            let peer = PeerId::random();
            let key = Key::from(peer);
            let mut entry = table.entry(key.clone());
            entry.insert(KademliaPeer::new(peer, vec![], ConnectionType::Connected));
        }

        let target = Key::from(PeerId::random());
        let closest = table.closest(target.clone(), 60usize);
        let mut prev = None;

        for peer in &closest {
            if let Some(value) = prev {
                assert!(value < target.distance(&peer.key));
            }

            prev = Some(target.distance(&peer.key));
        }
    }

    // generate random peer that falls in to specified k-bucket.
    //
    // NOTE: the preimage of the generated `Key` doesn't match the `Key` itself
    fn random_peer(
        rng: &mut impl rand::Rng,
        own_key: Key<PeerId>,
        bucket_index: usize,
    ) -> (Key<PeerId>, PeerId) {
        let peer = PeerId::random();
        let distance = BucketIndex(bucket_index).rand_distance(rng);
        let key_bytes = own_key.for_distance(distance);

        (Key::from_bytes(key_bytes, peer), peer)
    }

    #[test]
    fn add_peer_to_empty_table() {
        let own_peer_id = PeerId::random();
        let own_key = Key::from(own_peer_id);
        let mut table = RoutingTable::new(own_key.clone());

        // verify that local peer id resolves to special entry
        assert_eq!(table.entry(own_key), KBucketEntry::LocalNode);

        let peer = PeerId::random();
        let key = Key::from(peer);
        let mut test = table.entry(key.clone());
        let addresses = vec![];

        assert!(std::matches!(test, KBucketEntry::Vacant(_)));
        test.insert(KademliaPeer::new(
            peer,
            addresses.clone(),
            ConnectionType::Connected,
        ));

        assert_eq!(
            table.entry(key.clone()),
            KBucketEntry::Occupied(&mut KademliaPeer::new(
                peer,
                addresses.clone(),
                ConnectionType::Connected,
            ))
        );

        match table.entry(key.clone()) {
            KBucketEntry::Occupied(entry) => {
                entry.connection = ConnectionType::NotConnected;
            }
            state => panic!("invalid state for `KBucketEntry`"),
        }

        assert_eq!(
            table.entry(key.clone()),
            KBucketEntry::Occupied(&mut KademliaPeer::new(
                peer,
                addresses,
                ConnectionType::NotConnected,
            ))
        );
    }

    #[test]
    fn full_k_bucket() {
        let mut rng = rand::thread_rng();
        let own_peer_id = PeerId::random();
        let own_key = Key::from(own_peer_id);
        let mut table = RoutingTable::new(own_key.clone());

        // add 20 nodes to the same k-bucket
        for i in 0..20 {
            let (key, peer) = random_peer(&mut rng, own_key.clone(), 254);
            let mut entry = table.entry(key.clone());

            assert!(std::matches!(entry, KBucketEntry::Vacant(_)));
            entry.insert(KademliaPeer::new(peer, vec![], ConnectionType::Connected));
        }

        // try to add another peer and verify the peer is rejected
        // because the k-bucket is full of connected nodes
        let peer = PeerId::random();
        let distance = BucketIndex(254).rand_distance(&mut rng);
        let key_bytes = own_key.for_distance(distance);
        let key = Key::from_bytes(key_bytes, peer);

        let mut entry = table.entry(key.clone());
        assert!(std::matches!(entry, KBucketEntry::NoSlot));
    }

    #[test]
    fn peer_disconnects_and_is_evicted() {
        let mut rng = rand::thread_rng();
        let own_peer_id = PeerId::random();
        let own_key = Key::from(own_peer_id);
        let mut table = RoutingTable::new(own_key.clone());

        // add 20 nodes to the same k-bucket
        let peers = (0..20)
            .map(|_| {
                let (key, peer) = random_peer(&mut rng, own_key.clone(), 253);
                let mut entry = table.entry(key.clone());

                assert!(std::matches!(entry, KBucketEntry::Vacant(_)));
                entry.insert(KademliaPeer::new(peer, vec![], ConnectionType::Connected));

                (peer, key)
            })
            .collect::<Vec<_>>();

        // try to add another peer and verify the peer is rejected
        // because the k-bucket is full of connected nodes
        let peer = PeerId::random();
        let distance = BucketIndex(253).rand_distance(&mut rng);
        let key_bytes = own_key.for_distance(distance);
        let key = Key::from_bytes(key_bytes, peer);

        let mut entry = table.entry(key.clone());
        assert!(std::matches!(entry, KBucketEntry::NoSlot));

        // disconnect random peer
        match table.entry(peers[3].1.clone()) {
            KBucketEntry::Occupied(entry) => {
                entry.connection = ConnectionType::NotConnected;
            }
            _ => panic!("invalid state for node"),
        }

        // try to add the previously rejected peer again and verify it's added
        let mut entry = table.entry(key.clone());
        assert!(std::matches!(entry, KBucketEntry::Vacant(_)));
        entry.insert(KademliaPeer::new(
            peer,
            vec!["/ip6/::1/tcp/8888".parse().unwrap()],
            ConnectionType::CanConnect,
        ));

        // verify the node is still there
        let mut entry = table.entry(key.clone());
        let addresses = vec!["/ip6/::1/tcp/8888".parse().unwrap()];
        assert_eq!(
            entry,
            KBucketEntry::Occupied(&mut KademliaPeer::new(
                peer,
                addresses,
                ConnectionType::CanConnect,
            ))
        );
    }

    #[test]
    fn disconnected_peers_are_not_evicted_if_there_is_capacity() {
        let mut rng = rand::thread_rng();
        let own_peer_id = PeerId::random();
        let own_key = Key::from(own_peer_id);
        let mut table = RoutingTable::new(own_key.clone());

        // add 19 disconnected nodes to the same k-bucket
        let peers = (0..19)
            .map(|_| {
                let (key, peer) = random_peer(&mut rng, own_key.clone(), 252);
                let mut entry = table.entry(key.clone());

                assert!(std::matches!(entry, KBucketEntry::Vacant(_)));
                entry.insert(KademliaPeer::new(
                    peer,
                    vec![],
                    ConnectionType::NotConnected,
                ));

                (peer, key)
            })
            .collect::<Vec<_>>();

        // try to add another peer and verify it's accepted as there is
        // still room in the k-bucket for the node
        let peer = PeerId::random();
        let distance = BucketIndex(252).rand_distance(&mut rng);
        let key_bytes = own_key.for_distance(distance);
        let key = Key::from_bytes(key_bytes, peer);

        let mut entry = table.entry(key.clone());
        assert!(std::matches!(entry, KBucketEntry::Vacant(_)));
        entry.insert(KademliaPeer::new(
            peer,
            vec!["/ip6/::1/tcp/8888".parse().unwrap()],
            ConnectionType::CanConnect,
        ));
    }
}
