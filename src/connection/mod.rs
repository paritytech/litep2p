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

use crate::peer_id::PeerId;

use multiaddr::Multiaddr;
use parking_lot::Mutex;

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// Connection limit.
pub const MAX_CONNECTIONS: usize = 10_000;

/// [`ConnectionManager`] configuration.
pub struct Config {
    /// Maximum connections.
    pub max_connections: usize,
}

/// Handle for communicating with [`ConnectionManager`].
pub struct ConnectionManagerHandle {
    /// Connected peers.
    connected: Arc<Mutex<HashMap<PeerId, HashSet<Multiaddr>>>>,

    /// Known but unconnected peers.
    unconnected: Arc<Mutex<HashMap<PeerId, HashSet<Multiaddr>>>>,
}

impl ConnectionManagerHandle {
    /// Create new [`ConnectionManagerHandle`].
    pub fn new(
        connected: Arc<Mutex<HashMap<PeerId, HashSet<Multiaddr>>>>,
        unconnected: Arc<Mutex<HashMap<PeerId, HashSet<Multiaddr>>>>,
    ) -> Self {
        Self {
            connected,
            unconnected,
        }
    }

    /// Add one or more known addresses for peer.
    ///
    /// If peer doesn't exist, it will be added to known peers.
    pub fn add_know_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>) {
        let mut connected = self.connected.lock();

        match connected.get_mut(&peer) {
            Some(known_addresses) => known_addresses.extend(addresses),
            None => {
                drop(connected);

                let mut unconnected = self.unconnected.lock();
                unconnected.entry(*peer).or_default().extend(addresses);
            }
        }
    }

    /// Dial peer using `PeerId`.
    ///
    /// Returns an error if the peer is unknown.
    pub async fn dial(&self, peer: PeerId) -> crate::Result<()> {
        Ok(())
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        Ok(())
    }
}

/// Litep2p connection manager.
pub struct ConnectionManager {
    /// Connected peers.
    connected: Arc<Mutex<HashMap<PeerId, HashSet<Multiaddr>>>>,

    /// Known but unconnected peers.
    unconnected: Arc<Mutex<HashMap<PeerId, HashSet<Multiaddr>>>>,
}

impl ConnectionManager {
    /// Create new [`ConnectionManager`].
    pub fn new() -> (Self, ConnectionManagerHandle) {
        let connected = Arc::new(Mutex::new(HashMap::new()));
        let unconnected = Arc::new(Mutex::new(HashMap::new()));
        let handle = ConnectionManagerHandle::new(Arc::clone(&connected), Arc::clone(&unconnected));

        (
            Self {
                connected,
                unconnected,
            },
            handle,
        )
    }
}
