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

//! Memory store implementation for Kademlia.

use crate::protocol::libp2p::kademlia::record::{Key, Record};

use std::collections::HashMap;

/// Memory store events.
pub enum MemoryStoreEvent {}

/// Memory store.
pub struct MemoryStore {
    /// Records.
    records: HashMap<Key, Record>,
}

impl MemoryStore {
    /// Create new [`MemoryStore`].
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
        }
    }

    /// Try to get record from local store for `key`.
    fn get(&self, key: &Key) -> Option<&Record> {
        self.records.get(key)
    }

    /// Store `value` to local store under `key`.
    fn put(&mut self, key: Key, value: Record) {
        self.records.insert(key, value);
    }

    /// Poll next event from the store.
    async fn next_event() -> Option<MemoryStoreEvent> {
        None
    }
}
