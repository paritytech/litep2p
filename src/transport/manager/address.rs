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

use crate::{types::ConnectionId, PeerId};

use std::collections::HashMap;

use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;

/// Maximum number of addresses tracked for a peer.
const MAX_ADDRESSES: usize = 32;

#[allow(clippy::derived_hash_with_manual_eq)]
#[derive(Debug, Clone, Hash)]
pub struct AddressRecord {
    /// Address score.
    score: i32,

    /// Address.
    address: Multiaddr,

    /// Connection ID, if specified.
    connection_id: Option<ConnectionId>,
}

impl AsRef<Multiaddr> for AddressRecord {
    fn as_ref(&self) -> &Multiaddr {
        &self.address
    }
}

impl AddressRecord {
    /// Create new `AddressRecord` and if `address` doesn't contain `P2p`,
    /// append the provided `PeerId` to the address.
    pub fn new(
        peer: &PeerId,
        address: Multiaddr,
        score: i32,
        connection_id: Option<ConnectionId>,
    ) -> Self {
        let address = if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
            address.with(Protocol::P2p(
                Multihash::from_bytes(&peer.to_bytes()).expect("valid peer id"),
            ))
        } else {
            address
        };

        Self {
            address,
            score,
            connection_id,
        }
    }

    /// Create `AddressRecord` from `Multiaddr`.
    ///
    /// If `address` doesn't contain `PeerId`, return `None` to indicate that this
    /// an invalid `Multiaddr` from the perspective of the `TransportManager`.
    pub fn from_multiaddr(address: Multiaddr) -> Option<AddressRecord> {
        if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
            return None;
        }

        Some(AddressRecord {
            address,
            score: 0i32,
            connection_id: None,
        })
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

    /// Get connection ID.
    pub fn connection_id(&self) -> &Option<ConnectionId> {
        &self.connection_id
    }

    /// Update score of an address.
    pub fn update_score(&mut self, score: i32) {
        self.score = self.score.saturating_add(score);
    }

    /// Set `ConnectionId` for the [`AddressRecord`].
    pub fn set_connection_id(&mut self, connection_id: ConnectionId) {
        self.connection_id = Some(connection_id);
    }
}

impl PartialEq for AddressRecord {
    fn eq(&self, other: &Self) -> bool {
        self.score.eq(&other.score)
    }
}

impl Eq for AddressRecord {}

impl PartialOrd for AddressRecord {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.score.cmp(&other.score))
    }
}

impl Ord for AddressRecord {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.score.cmp(&other.score)
    }
}

/// Store for peer addresses.
#[derive(Debug)]
pub struct AddressStore {
    /// Addresses available.
    pub addresses: HashMap<Multiaddr, AddressRecord>,
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
        }
    }

    /// Check if [`AddressStore`] is empty.
    pub fn is_empty(&self) -> bool {
        self.addresses.is_empty()
    }

    /// Check if address is already in the address store.
    pub fn contains(&self, address: &Multiaddr) -> bool {
        self.addresses.contains_key(address)
    }

    /// Update the address record into [`AddressStore`] with the provided score.
    ///
    /// If the address is not in the store, it will be inserted.
    pub fn insert(&mut self, record: AddressRecord) {
        match self.addresses.entry(record.address.clone()) {
            std::collections::hash_map::Entry::Occupied(occupied_entry) => {
                let found_record = occupied_entry.into_mut();
                found_record.update_score(record.score);
                found_record.connection_id = record.connection_id;
            }
            std::collections::hash_map::Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(record.clone());
            }
        }
    }

    /// Update the score of an existing address.
    pub fn update(&mut self, address: &Multiaddr, score: i32) {
        if let Some(record) = self.addresses.get_mut(address) {
            record.update_score(score);
        }
    }

    /// Return the available addresses sorted by score.
    pub fn addresses(&self) -> Vec<AddressRecord> {
        let mut records = self.addresses.values().cloned().collect::<Vec<_>>();
        records.sort_by(|lhs, rhs| rhs.score.cmp(&lhs.score));
        records
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
                rng.gen_range(1..=255),
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
            ),
            rng.gen_range(1..=65535),
        ));
        let score: i32 = rng.gen();

        AddressRecord::new(
            &peer,
            Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Tcp(address.port())),
            score,
            None,
        )
    }

    fn ws_address_record(rng: &mut ThreadRng) -> AddressRecord {
        let peer = PeerId::random();
        let address = std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(
                rng.gen_range(1..=255),
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
            ),
            rng.gen_range(1..=65535),
        ));
        let score: i32 = rng.gen();

        AddressRecord::new(
            &peer,
            Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Tcp(address.port()))
                .with(Protocol::Ws(std::borrow::Cow::Owned("/".to_string()))),
            score,
            None,
        )
    }

    fn quic_address_record(rng: &mut ThreadRng) -> AddressRecord {
        let peer = PeerId::random();
        let address = std::net::SocketAddr::V4(SocketAddrV4::new(
            Ipv4Addr::new(
                rng.gen_range(1..=255),
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
                rng.gen_range(0..=255),
            ),
            rng.gen_range(1..=65535),
        ));
        let score: i32 = rng.gen();

        AddressRecord::new(
            &peer,
            Multiaddr::empty()
                .with(Protocol::from(address.ip()))
                .with(Protocol::Udp(address.port()))
                .with(Protocol::QuicV1),
            score,
            None,
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

        let known_addresses = store.by_address.len();
        assert!(known_addresses >= 3);

        let taken = store.take(known_addresses - 2);
        assert_eq!(known_addresses - 2, taken.len());
        assert!(!store.is_empty());

        let mut prev: Option<AddressRecord> = None;
        for record in taken {
            assert!(!store.contains(record.address()));

            if let Some(previous) = prev {
                assert!(previous.score > record.score);
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

        assert_eq!(store.by_address.len(), 3);

        let taken = store.take(8usize);
        assert_eq!(taken.len(), 3);
        assert!(store.is_empty());

        let mut prev: Option<AddressRecord> = None;
        for record in taken {
            if prev.is_none() {
                prev = Some(record);
            } else {
                assert!(prev.unwrap().score > record.score);
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

        for record in store.by_score {
            let stored = cloned.get(record.address()).unwrap();
            assert_eq!(stored.score(), record.score());
            assert_eq!(stored.connection_id(), record.connection_id());
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

        for record in store.by_score {
            let stored = cloned.get(record.address()).unwrap();
            assert_eq!(stored.score(), record.score());
            assert_eq!(stored.connection_id(), record.connection_id());
            assert_eq!(stored.address(), record.address());
        }
    }
}
