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
}
