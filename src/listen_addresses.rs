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

use multiaddr::Multiaddr;
use parking_lot::RwLock;

/// Set of the public addresses of the local node.
///
/// These addresses are reported to the identify protocol.
#[derive(Debug, Clone)]
pub struct IdentifyPublicAddresses {
    inner: Arc<RwLock<HashSet<Multiaddr>>>,
}

impl IdentifyPublicAddresses {
    /// Create new [`IdentifyPublicAddresses`].
    fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashSet::new())),
        }
    }

    /// Add public address.
    pub fn add(&self, address: Multiaddr) {
        if address.is_empty() {
            return;
        }

        self.inner.write().insert(address);
    }

    /// Remove public address.
    pub fn remove(&self, address: &Multiaddr) {
        self.inner.write().remove(address);
    }

    /// Get public addresses.
    pub fn get_addresses(&self) -> Vec<Multiaddr> {
        self.inner.read().iter().cloned().collect()
    }

    /// Get public addresses in the identify raw format.
    fn get_raw_addresses(&self) -> Vec<Vec<u8>> {
        self.inner.read().iter().map(|address| address.to_vec()).collect()
    }

    /// Extend public addresses.
    pub fn extend(&self, addresses: impl IntoIterator<Item = Multiaddr>) {
        self.inner
            .write()
            .extend(addresses.into_iter().filter(|address| !address.is_empty()));
    }
}
