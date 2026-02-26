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

use crate::{error::DialError, PeerId};

use ip_network::IpNetwork;
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;

use std::collections::{hash_map::Entry, HashMap};

/// Maximum number of addresses tracked for a peer.
const MAX_ADDRESSES: usize = 64;

/// Scores for address records.
pub mod scores {
    /// Score indicating that the connection was successfully established.
    pub const CONNECTION_ESTABLISHED: i32 = 100i32;

    /// Score for failing to connect due to an invalid or unreachable address.
    pub const CONNECTION_FAILURE: i32 = -100i32;

    /// Score for providing an invalid address.
    ///
    /// This address can never be reached and is effectively banned.
    pub const ADDRESS_FAILURE: i32 = i32::MIN;

    /// Initial score for public/global addresses.
    ///
    /// This gives public addresses a slight priority over private addresses
    /// when all addresses are untested (private addresses start at 0).
    pub const PUBLIC_ADDRESS_BONUS: i32 = 1i32;
}

#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Debug, Clone, Hash)]
pub struct AddressRecord {
    /// Address score.
    score: i32,

    /// Address.
    address: Multiaddr,
}

impl AsRef<Multiaddr> for AddressRecord {
    fn as_ref(&self) -> &Multiaddr {
        &self.address
    }
}

impl AddressRecord {
    /// Create new `AddressRecord` and if `address` doesn't contain `P2p`,
    /// append the provided `PeerId` to the address.
    pub fn new(peer: &PeerId, address: Multiaddr, score: i32) -> Self {
        let address = if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
            address.with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).expect("valid peer id"),
            ))
        } else {
            address
        };

        Self::from_raw_multiaddr_with_score(address, score)
    }

    /// Create `AddressRecord` from `Multiaddr`.
    ///
    /// If `address` doesn't contain `PeerId`, return `None` to indicate that this
    /// an invalid `Multiaddr` from the perspective of the `TransportManager`.
    pub fn from_multiaddr(address: Multiaddr) -> Option<AddressRecord> {
        if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
            return None;
        }

        Some(Self::from_raw_multiaddr_with_score(address, 0))
    }

    /// Create `AddressRecord` from `Multiaddr`.
    ///
    /// This method does not check if the address contains `PeerId`.
    ///
    /// Please consider using [`Self::from_multiaddr`] from the transport manager code.
    pub fn from_raw_multiaddr(address: Multiaddr) -> AddressRecord {
        Self::from_raw_multiaddr_with_score(address, 0)
    }

    /// Create `AddressRecord` from `Multiaddr`.
    ///
    /// This method does not check if the address contains `PeerId`.
    ///
    /// Please consider using [`Self::from_multiaddr`] from the transport manager code.
    pub fn from_raw_multiaddr_with_score(address: Multiaddr, score: i32) -> AddressRecord {
        Self { address, score }
    }

    /// Get address score.
    #[cfg(test)]
    pub fn score(&self) -> i32 {
        self.score
    }

    /// Get address.
    pub fn address(&self) -> &Multiaddr {
        &self.address
    }

    /// Update score of an address.
    pub fn update_score(&mut self, score: i32) {
        self.score = score;
    }
}

/// Check if a multiaddr represents a global/public address.
///
/// DNS addresses are considered potentially public.
fn is_global_multiaddr(address: &Multiaddr) -> bool {
    for protocol in address.iter() {
        match protocol {
            Protocol::Ip4(ip) => return IpNetwork::from(ip).is_global(),
            Protocol::Ip6(ip) => return IpNetwork::from(ip).is_global(),
            // DNS addresses could resolve to public IPs, treat as potentially public.
            // Ideally we need to resolve DNS to check the actual IPs. However, this
            // is a more complex operation that requires async DNS resolution in the
            // transport manager context / transport layer.
            Protocol::Dns(_) | Protocol::Dns4(_) | Protocol::Dns6(_) => return true,
            _ => continue,
        }
    }

    // Consider the address as non-global if no IP or DNS component is found
    false
}

impl PartialEq for AddressRecord {
    fn eq(&self, other: &Self) -> bool {
        self.score.eq(&other.score)
    }
}

impl Eq for AddressRecord {}

impl PartialOrd for AddressRecord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AddressRecord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.cmp(&other.score)
    }
}

/// Store for peer addresses.
#[derive(Debug, Clone, Default)]
pub struct AddressStore {
    /// Addresses available.
    pub addresses: HashMap<Multiaddr, AddressRecord>,
    /// Maximum capacity of the address store.
    max_capacity: usize,
}

impl FromIterator<Multiaddr> for AddressStore {
    fn from_iter<T: IntoIterator<Item = Multiaddr>>(iter: T) -> Self {
        let mut store = AddressStore::new();
        for address in iter {
            if let Some(record) = AddressRecord::from_multiaddr(address) {
                store.insert(record);
            }
        }

        store
    }
}

impl FromIterator<AddressRecord> for AddressStore {
    fn from_iter<T: IntoIterator<Item = AddressRecord>>(iter: T) -> Self {
        let mut store = AddressStore::new();
        for record in iter {
            store.insert(record);
        }

        store
    }
}

impl Extend<AddressRecord> for AddressStore {
    fn extend<T: IntoIterator<Item = AddressRecord>>(&mut self, iter: T) {
        for record in iter {
            self.insert(record)
        }
    }
}

impl<'a> Extend<&'a AddressRecord> for AddressStore {
    fn extend<T: IntoIterator<Item = &'a AddressRecord>>(&mut self, iter: T) {
        for record in iter {
            self.insert(record.clone())
        }
    }
}

impl AddressStore {
    /// Create new [`AddressStore`].
    pub fn new() -> Self {
        Self {
            addresses: HashMap::with_capacity(MAX_ADDRESSES),
            max_capacity: MAX_ADDRESSES,
        }
    }

    /// Get the score for a given error.
    pub fn error_score(error: &DialError) -> i32 {
        match error {
            DialError::AddressError(_) => scores::ADDRESS_FAILURE,
            _ => scores::CONNECTION_FAILURE,
        }
    }

    /// Check if [`AddressStore`] is empty.
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    /// Insert the address record into [`AddressStore`] with the provided score.
    ///
    /// If the address is not in the store, it will be inserted with a bonus for public addresses.
    /// Otherwise, the score will be updated only for connection events (non-zero scores),
    /// not for re-adding the same address which should not overwrite connection history.
    pub fn insert(&mut self, record: AddressRecord) {
        let is_public = is_global_multiaddr(&record.address);

        if let Entry::Occupied(mut occupied) = self.addresses.entry(record.address.clone()) {
            // Only update score for connection events (non-zero scores).
            // Re-adding an address (score 0) via rediscovery should not wipe out
            // connection success/failure history.
            if record.score != 0 {
                let score = if is_public {
                    record.score.saturating_add(scores::PUBLIC_ADDRESS_BONUS)
                } else {
                    record.score
                };
                occupied.get_mut().update_score(score);
            }
            return;
        }

        // Reward public addresses with a bonus.
        let record = if is_public {
            AddressRecord {
                score: record.score.saturating_add(scores::PUBLIC_ADDRESS_BONUS),
                ..record
            }
        } else {
            record
        };

        // The eviction algorithm favours addresses with higher scores.
        //
        // This algorithm has the following implications:
        //  - it keeps the best addresses in the store.
        //  - if the store is at capacity, the worst address will be evicted.
        //  - an address that is not dialed yet (with score zero) will be preferred over an address
        //  that already failed (with negative score).
        if self.addresses.len() >= self.max_capacity {
            let min_record = self
                .addresses
                .values()
                .min()
                .cloned()
                .expect("There is at least one element checked above; qed");

            // The lowest score is better than the new record.
            if record.score < min_record.score {
                return;
            }
            self.addresses.remove(min_record.address());
        }

        // Insert the record.
        self.addresses.insert(record.address.clone(), record);
    }

    /// Return the available addresses sorted by score.
    pub fn addresses(&self, limit: usize) -> Vec<Multiaddr> {
        let mut records = self.addresses.values().cloned().collect::<Vec<_>>();
        records.sort_by(|lhs, rhs| rhs.score.cmp(&lhs.score));
        records.into_iter().take(limit).map(|record| record.address).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        net::{Ipv4Addr, SocketAddrV4},
    };

    use super::*;
    use rand::{rngs::ThreadRng, Rng};

    fn tcp_address_record(rng: &mut ThreadRng) -> AddressRecord {
        let peer = PeerId::random();
        let address = std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(
                10,
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
                rng.gen_range(1..=255),
            ),
            rng.gen_range(1..=65535),
        ));
        let score: i32 = rng.gen_range(10..=200);

        AddressRecord::new(
            &peer,
            Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Tcp(address.port())),
            score,
        )
    }

    fn ws_address_record(rng: &mut ThreadRng) -> AddressRecord {
        let peer = PeerId::random();
        let address = std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(
                10,
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
                rng.gen_range(1..=255),
            ),
            rng.gen_range(1..=65535),
        ));
        let score: i32 = rng.gen_range(10..=200);

        AddressRecord::new(
            &peer,
            Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Tcp(address.port()))
                .with(Protocol::Ws(std::borrow::Cow::Owned("/".to_string()))),
            score,
        )
    }

    fn quic_address_record(rng: &mut ThreadRng) -> AddressRecord {
        let peer = PeerId::random();
        let address = std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(
                10,
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
                rng.gen_range(1..=255),
            ),
            rng.gen_range(1..=65535),
        ));
        let score: i32 = rng.gen_range(10..=200);

        AddressRecord::new(
            &peer,
            Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Udp(address.port()))
                .with(Protocol::QuicV1),
            score,
        )
    }

    #[test]
    fn take_multiple_records() {
        let mut store = AddressStore::new();
        let mut rng = rand::thread_rng();

        for _ in 0..rng.gen_range(1..5) {
            store.insert(tcp_address_record(&mut rng));
        }
        for _ in 0..rng.gen_range(1..5) {
            store.insert(ws_address_record(&mut rng));
        }
        for _ in 0..rng.gen_range(1..5) {
            store.insert(quic_address_record(&mut rng));
        }

        let known_addresses = store.addresses.len();
        assert!(known_addresses >= 3);

        let taken = store.addresses(known_addresses - 2);
        assert_eq!(known_addresses - 2, taken.len());
        assert!(!store.is_empty());

        let mut prev: Option<AddressRecord> = None;
        for address in taken {
            // Addresses are still in the store.
            assert!(store.addresses.contains_key(&address));

            let record = store.addresses.get(&address).unwrap().clone();

            if let Some(previous) = prev {
                assert!(previous.score >= record.score);
            }

            prev = Some(record);
        }
    }

    #[test]
    fn attempt_to_take_excess_records() {
        let mut store = AddressStore::new();
        let mut rng = rand::thread_rng();

        store.insert(tcp_address_record(&mut rng));
        store.insert(ws_address_record(&mut rng));
        store.insert(quic_address_record(&mut rng));

        assert_eq!(store.addresses.len(), 3);

        let taken = store.addresses(8usize);
        assert_eq!(taken.len(), 3);

        let mut prev: Option<AddressRecord> = None;
        for record in taken {
            let record = store.addresses.get(&record).unwrap().clone();

            if prev.is_none() {
                prev = Some(record);
            } else {
                assert!(prev.unwrap().score >= record.score);
                prev = Some(record);
            }
        }
    }

    #[test]
    fn extend_from_iterator() {
        let mut store = AddressStore::new();
        let mut rng = rand::thread_rng();

        let records = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    tcp_address_record(&mut rng)
                } else if i % 3 == 0 {
                    quic_address_record(&mut rng)
                } else {
                    ws_address_record(&mut rng)
                }
            })
            .collect::<Vec<_>>();

        assert!(store.is_empty());
        let cloned = records
            .iter()
            .cloned()
            .map(|record| (record.address().clone(), record))
            .collect::<HashMap<_, _>>();
        store.extend(records);

        for record in store.addresses.values() {
            let stored = cloned.get(record.address()).unwrap();
            assert_eq!(stored.score(), record.score());
            assert_eq!(stored.address(), record.address());
        }
    }

    #[test]
    fn extend_from_iterator_ref() {
        let mut store = AddressStore::new();
        let mut rng = rand::thread_rng();

        let records = (0..10)
            .map(|i| {
                if i % 2 == 0 {
                    let record = tcp_address_record(&mut rng);
                    (record.address().clone(), record)
                } else if i % 3 == 0 {
                    let record = quic_address_record(&mut rng);
                    (record.address().clone(), record)
                } else {
                    let record = ws_address_record(&mut rng);
                    (record.address().clone(), record)
                }
            })
            .collect::<Vec<_>>();

        assert!(store.is_empty());
        let cloned = records.iter().cloned().collect::<HashMap<_, _>>();
        store.extend(records.iter().map(|(_, record)| record));

        for record in store.addresses.values() {
            let stored = cloned.get(record.address()).unwrap();
            assert_eq!(stored.score(), record.score());
            assert_eq!(stored.address(), record.address());
        }
    }

    #[test]
    fn insert_record() {
        let mut store = AddressStore::new();
        let mut rng = rand::thread_rng();

        let mut record = tcp_address_record(&mut rng);
        record.score = 10;

        store.insert(record.clone());

        assert_eq!(store.addresses.len(), 1);
        assert_eq!(store.addresses.get(record.address()).unwrap(), &record);

        // This time the record score is replaced (not accumulated).
        store.insert(record.clone());

        assert_eq!(store.addresses.len(), 1);
        let store_record = store.addresses.get(record.address()).unwrap();
        assert_eq!(store_record.score, record.score);
    }

    #[test]
    fn insert_record_does_not_accumulate_public_bonus() {
        let mut store = AddressStore::new();
        let peer = PeerId::random();

        // Create a public address (8.8.8.8 is global) using from_multiaddr.
        // The bonus is NOT applied at construction time, only when first inserted.
        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(8, 8, 8, 8)))
            .with(Protocol::Tcp(9999))
            .with(Protocol::P2p(
                multihash::Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        let record = AddressRecord::from_multiaddr(address.clone()).unwrap();
        assert_eq!(record.score, 0);

        store.insert(record.clone());
        assert_eq!(store.addresses.len(), 1);
        // Bonus applied on first insert.
        assert_eq!(
            store.addresses.get(&address).unwrap().score,
            scores::PUBLIC_ADDRESS_BONUS
        );

        // Re-adding the same address should NOT accumulate the bonus.
        let record2 = AddressRecord::from_multiaddr(address.clone()).unwrap();
        store.insert(record2);

        assert_eq!(store.addresses.len(), 1);
        // Score should still be 1, not 2.
        assert_eq!(
            store.addresses.get(&address).unwrap().score,
            scores::PUBLIC_ADDRESS_BONUS
        );

        // However, connection events should still update (replace) the score.
        let connection_record =
            AddressRecord::new(&peer, address.clone(), scores::CONNECTION_ESTABLISHED);
        store.insert(connection_record);

        assert_eq!(store.addresses.len(), 1);
        // Score should now be CONNECTION_ESTABLISHED + PUBLIC_ADDRESS_BONUS.
        assert_eq!(
            store.addresses.get(&address).unwrap().score,
            scores::CONNECTION_ESTABLISHED + scores::PUBLIC_ADDRESS_BONUS
        );
    }

    #[test]
    fn rediscovery_does_not_wipe_dial_failure() {
        let mut store = AddressStore::new();
        let peer = PeerId::random();

        // Public address (8.8.8.8 is global).
        let address = Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(8, 8, 8, 8)))
            .with(Protocol::Tcp(9999))
            .with(Protocol::P2p(
                multihash::Multihash::from_bytes(&peer.to_bytes()).unwrap(),
            ));

        // First, add the address normally.
        let record = AddressRecord::from_multiaddr(address.clone()).unwrap();
        store.insert(record);
        assert_eq!(
            store.addresses.get(&address).unwrap().score,
            scores::PUBLIC_ADDRESS_BONUS
        );

        // Dial failure occurs.
        let failure_record = AddressRecord::new(&peer, address.clone(), scores::CONNECTION_FAILURE);
        store.insert(failure_record);
        let failure_score = scores::CONNECTION_FAILURE + scores::PUBLIC_ADDRESS_BONUS;
        assert_eq!(store.addresses.get(&address).unwrap().score, failure_score);

        // Address is rediscovered via Kademlia (creates record with score 0).
        // This should NOT wipe out the dial failure score.
        let rediscovered = AddressRecord::from_multiaddr(address.clone()).unwrap();
        assert_eq!(rediscovered.score, 0);
        store.insert(rediscovered);

        // Score should still reflect the failure, not 0.
        assert_eq!(store.addresses.get(&address).unwrap().score, failure_score);
    }

    #[test]
    fn evict_on_capacity() {
        let mut store = AddressStore {
            addresses: HashMap::new(),
            max_capacity: 2,
        };

        let mut rng = rand::thread_rng();
        let mut first_record = tcp_address_record(&mut rng);
        first_record.score = scores::CONNECTION_ESTABLISHED;
        let mut second_record = ws_address_record(&mut rng);
        second_record.score = 0;

        store.insert(first_record.clone());
        store.insert(second_record.clone());

        assert_eq!(store.addresses.len(), 2);

        // We have better addresses, ignore this one.
        let mut third_record = quic_address_record(&mut rng);
        third_record.score = scores::CONNECTION_FAILURE;
        store.insert(third_record.clone());
        assert_eq!(store.addresses.len(), 2);
        assert!(store.addresses.contains_key(first_record.address()));
        assert!(store.addresses.contains_key(second_record.address()));

        // Evict the address with the lowest score.
        // Store contains scores: [100, 0].
        let mut fourth_record = quic_address_record(&mut rng);
        fourth_record.score = 1;
        store.insert(fourth_record.clone());

        assert_eq!(store.addresses.len(), 2);
        assert!(store.addresses.contains_key(first_record.address()));
        assert!(store.addresses.contains_key(fourth_record.address()));
    }
}
