// Copyright 2024 litep2p developers
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

use std::{collections::HashSet, sync::Arc};

use multiaddr::{Multiaddr, Protocol};
use parking_lot::RwLock;

use crate::PeerId;

/// Set of the public addresses of the local node.
///
/// These addresses are reported to the identify protocol.
#[derive(Debug, Clone)]
pub struct ListenAddresses {
    inner: Arc<RwLock<HashSet<Multiaddr>>>,
    local_peer_id: PeerId,
}

impl ListenAddresses {
    /// Creates new [`ListenAddresses`] from the given peer ID.
    pub(crate) fn new(local_peer_id: PeerId) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashSet::new())),
            local_peer_id,
        }
    }

    /// Add a public address to the list of listen addresses.
    ///
    /// The address must contain the local peer ID, otherwise the address will be modified to
    /// include the local peer ID.
    ///
    /// Returns true if the address was added, false if it was already present.
    pub fn register_listen_address(&self, address: Multiaddr) -> Result<bool, Multiaddr> {
        let address = self.add_local_peer(address)?;
        Ok(self.inner.write().insert(address))
    }

    /// Remove public address.
    pub fn remove(&self, address: &Multiaddr) -> bool {
        self.inner.write().remove(address)
    }

    /// Returns `true` if the set contains the given address.
    ///
    /// The address must contain the local peer ID.
    pub fn contains(&self, address: &Multiaddr) -> bool {
        self.inner.read().contains(address)
    }

    /// Returns a vector of the available listen addresses.
    pub fn get_addresses(&self) -> Vec<Multiaddr> {
        self.inner.read().iter().cloned().collect()
    }

    /// Returns an immutable reference to the set of listen addresses.
    ///
    /// This method can be used when `get_addresses` is not sufficient.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use litep2p::listen_addresses::ListenAddresses;
    /// #
    /// # fn listen_addresses(addresses: ListenAddresses) {
    ///   let string_addresses = addresses.locked().iter().map(|address| address.to_string()).collect::<Vec<_>>();
    /// # }
    /// ```
    pub fn locked(&self) -> LockedListenAddresses {
        LockedListenAddresses {
            inner: self.inner.read(),
        }
    }

    /// Extend public addresses.
    pub fn extend(&self, addresses: impl IntoIterator<Item = Multiaddr>) {
        self.inner
            .write()
            .extend(addresses.into_iter().filter_map(|address| self.add_local_peer(address).ok()));
    }

    /// Returns the number of listen addresses.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }

    /// Returns `true` if the set of listen addresses is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.read().is_empty()
    }

    /// Modify the provided address to contain the local peer ID.
    fn add_local_peer(&self, mut address: Multiaddr) -> Result<Multiaddr, Multiaddr> {
        if address.is_empty() {
            return Err(address);
        }

        // Verify the peer ID from the address corresponds to the local peer ID.
        if let Some(peer_id) = PeerId::try_from_multiaddr(&address) {
            if peer_id != self.local_peer_id {
                return Err(address);
            }
        } else {
            address.push(Protocol::P2p(self.local_peer_id.into()));
        }

        Ok(address)
    }
}

/// A short lived instance of the locked listen addresses.
pub struct LockedListenAddresses<'a> {
    inner: parking_lot::RwLockReadGuard<'a, HashSet<Multiaddr>>,
}

impl<'a> LockedListenAddresses<'a> {
    /// Iterate over the listen addresses.
    ///
    /// This exposes all the functionality of the standard iterator.
    pub fn iter(&self) -> impl Iterator<Item = &Multiaddr> {
        self.inner.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn add_remove_contains() {
        let peer_id = PeerId::random();
        let addresses = ListenAddresses::new(peer_id);
        let address = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();
        let peer_address = Multiaddr::from_str("/dns/domain1.com/tcp/30333")
            .unwrap()
            .with(Protocol::P2p(peer_id.into()));

        assert!(!addresses.contains(&address));

        assert!(addresses.register_listen_address(address.clone()).unwrap());
        // Adding the address a second time returns Ok(false).
        assert!(!addresses.register_listen_address(address.clone()).unwrap());

        assert!(!addresses.contains(&address));
        assert!(addresses.contains(&peer_address));

        addresses.remove(&peer_address);
        assert!(!addresses.contains(&peer_address));
    }

    #[test]
    fn get_addresses() {
        let peer_id = PeerId::random();
        let addresses = ListenAddresses::new(peer_id);
        let address1 = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();
        let address2 = Multiaddr::from_str("/dns/domain2.com/tcp/30333").unwrap();
        // Addresses different than the local peer ID are ignored.
        let address3 = Multiaddr::from_str(
            "/dns/domain2.com/tcp/30333/p2p/12D3KooWSueCPH3puP2PcvqPJdNaDNF3jMZjtJtDiSy35pWrbt5h",
        )
        .unwrap();

        assert!(addresses.register_listen_address(address1.clone()).unwrap());
        assert!(addresses.register_listen_address(address2.clone()).unwrap());
        addresses.register_listen_address(address3.clone()).unwrap_err();

        let addresses = addresses.get_addresses();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&address1.with(Protocol::P2p(peer_id.into()))));
        assert!(addresses.contains(&address2.with(Protocol::P2p(peer_id.into()))));
    }

    #[test]
    fn locked() {
        let peer_id = PeerId::random();
        let addresses = ListenAddresses::new(peer_id);
        let address1 = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();
        let address2 = Multiaddr::from_str(
            "/dns/domain2.com/tcp/30333/p2p/12D3KooWSueCPH3puP2PcvqPJdNaDNF3jMZjtJtDiSy35pWrbt5h",
        )
        .unwrap();

        assert!(addresses.register_listen_address(address1.clone()).unwrap());
        addresses.register_listen_address(address2.clone()).unwrap_err();

        let addresses = addresses.locked();
        let addresses = addresses.iter().map(|address| address.to_vec()).collect::<Vec<_>>();
        assert_eq!(addresses.len(), 1);
        assert!(addresses.contains(&address1.with(Protocol::P2p(peer_id.into())).to_vec()));
    }

    #[test]
    fn extend() {
        let peer_id = PeerId::random();
        let addresses = ListenAddresses::new(peer_id);
        let address1 = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();
        let address2 = Multiaddr::from_str("/dns/domain2.com/tcp/30333")
            .unwrap()
            .with(Protocol::P2p(peer_id.into()));
        // Addresses different than the local peer ID are ignored.
        let address3 = Multiaddr::from_str(
            "/dns/domain2.com/tcp/30333/p2p/12D3KooWSueCPH3puP2PcvqPJdNaDNF3jMZjtJtDiSy35pWrbt5h",
        )
        .unwrap();

        addresses.extend(vec![address1.clone(), address2.clone(), address3]);

        let addresses = addresses.get_addresses();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&address1.with(Protocol::P2p(peer_id.into()))));
        assert!(addresses.contains(&address2));
    }
}
