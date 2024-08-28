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

/// Set of the public addresses of the local node.
///
/// These addresses are reported to the identify protocol.
#[derive(Debug, Clone)]
pub struct ListenAddresses {
    inner: Arc<RwLock<HashSet<Multiaddr>>>,
}

impl ListenAddresses {
    /// Creates new [`ListenAddresses`] from the inner set.
    pub(crate) fn from_inner(inner: Arc<RwLock<HashSet<Multiaddr>>>) -> Self {
        Self { inner }
    }

    /// Add a public address to the list of listen addresses.
    ///
    /// # Note
    ///
    /// Users must ensure that the address is public.
    ///
    /// Empty addresses are ignored.
    pub fn register_listen_address(&self, address: Multiaddr) {
        if !address.iter().any(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
            return;
        }
        self.inner.write().insert(address);
    }

    /// Remove public address.
    pub fn remove(&self, address: &Multiaddr) {
        self.inner.write().remove(address);
    }

    /// Returns `true` if the set contains the given address.
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
            .extend(addresses.into_iter().filter(|address| !address.is_empty()));
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
        let addresses = ListenAddresses::new();
        let address = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();

        assert!(!addresses.contains(&address));
        addresses.add(address.clone());
        assert!(addresses.contains(&address));

        addresses.remove(&address);
        assert!(!addresses.contains(&address));
    }

    #[test]
    fn get_addresses() {
        let addresses = ListenAddresses::new();
        let address1 = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();
        let address2 = Multiaddr::from_str("/dns/domain2.com/tcp/30333").unwrap();

        addresses.add(address1.clone());
        addresses.add(address2.clone());

        let addresses = addresses.get_addresses();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&address1));
        assert!(addresses.contains(&address2));
    }

    #[test]
    fn locked() {
        let addresses = ListenAddresses::new();
        let address1 = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();
        let address2 = Multiaddr::from_str("/dns/domain2.com/tcp/30333").unwrap();

        addresses.add(address1.clone());
        addresses.add(address2.clone());

        let addresses = addresses.locked();
        let addresses = addresses.iter().map(|address| address.to_vec()).collect::<Vec<_>>();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&address1.to_vec()));
        assert!(addresses.contains(&address2.to_vec()));
    }

    #[test]
    fn extend() {
        let addresses = ListenAddresses::new();
        let address1 = Multiaddr::from_str("/dns/domain1.com/tcp/30333").unwrap();
        let address2 = Multiaddr::from_str("/dns/domain2.com/tcp/30333").unwrap();

        addresses.extend(vec![address1.clone(), address2.clone()]);

        let addresses = addresses.get_addresses();
        assert_eq!(addresses.len(), 2);
        assert!(addresses.contains(&address1));
        assert!(addresses.contains(&address2));
    }
}
