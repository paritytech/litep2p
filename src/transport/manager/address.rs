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

use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;

use std::collections::{BinaryHeap, HashSet};

#[derive(Debug, Clone, Hash)]
pub struct AddressRecord {
    /// Address score.
    score: i32,

    /// Address.
    address: Multiaddr,

    /// Connection ID, if specifed.
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
                Multihash::from_bytes(&peer.to_bytes()).ok().expect("valid peer id"),
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
        self.score += score;
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
    //// Addresses sorted by score.
    pub by_score: BinaryHeap<AddressRecord>,

    /// Addresses queryable by hashing them for faster lookup.
    pub by_address: HashSet<Multiaddr>,
}

impl FromIterator<Multiaddr> for AddressStore {
    fn from_iter<T: IntoIterator<Item = Multiaddr>>(iter: T) -> Self {
        let mut store = AddressStore::new();
        for address in iter {
            if let Some(address) = AddressRecord::from_multiaddr(address) {
                store.insert(address.into());
            }
        }

        store
    }
}

impl FromIterator<AddressRecord> for AddressStore {
    fn from_iter<T: IntoIterator<Item = AddressRecord>>(iter: T) -> Self {
        let mut store = AddressStore::new();
        for record in iter {
            store.by_address.insert(record.address.clone());
            store.by_score.push(record);
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
            by_score: BinaryHeap::new(),
            by_address: HashSet::new(),
        }
    }

    /// Check if [`AddressStore`] is empty.
    pub fn is_empty(&self) -> bool {
        self.by_score.is_empty()
    }

    /// Check if address is already in the a
    pub fn contains(&self, address: &Multiaddr) -> bool {
        self.by_address.contains(address)
    }

    /// Insert new address record into [`AddressStore`] with default address score.
    pub fn insert(&mut self, mut record: AddressRecord) {
        record.connection_id = None;
        self.by_address.insert(record.address.clone());
        self.by_score.push(record);
    }

    /// Insert new address into [`AddressStore`] with score.
    pub fn insert_with_score(&mut self, address: Multiaddr, score: i32) {
        self.by_address.insert(address.clone());
        self.by_score.push(AddressRecord {
            score,
            address,
            connection_id: None,
        });
    }

    /// Pop address with the highest score from [`AddressScore`].
    pub fn pop(&mut self) -> Option<AddressRecord> {
        self.by_score.pop().map(|record| {
            self.by_address.remove(&record.address);
            record
        })
    }

    /// Take at most `limit` `AddressRecord`s from [`AddressStore`].
    pub fn take(&mut self, limit: usize) -> Vec<AddressRecord> {
        let mut records = Vec::new();

        for _ in 0..limit {
            match self.pop() {
                Some(record) => records.push(record),
                None => break,
            }
        }

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
