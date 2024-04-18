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

#![allow(unused)]
use crate::protocol::libp2p::kademlia::record::{Key, Record};

use std::collections::{hash_map::Entry, HashMap};

/// Memory store events.
pub enum MemoryStoreEvent {}

/// Memory store.
pub struct MemoryStore {
    /// Records.
    records: HashMap<Key, Record>,
    /// Configuration.
    config: MemoryStoreConfig,
}

impl MemoryStore {
    /// Create new [`MemoryStore`].
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            config: MemoryStoreConfig::default(),
        }
    }

    /// Create new [`MemoryStore`] with the provided configuration.
    pub fn with_config(config: MemoryStoreConfig) -> Self {
        Self {
            records: HashMap::new(),
            config,
        }
    }

    /// Try to get record from local store for `key`.
    pub fn get(&self, key: &Key) -> Option<&Record> {
        self.records.get(key)
    }

    /// Store record.
    pub fn put(&mut self, record: Record) {
        if record.value.len() >= self.config.max_record_size_bytes {
            return;
        }

        let len = self.records.len();
        match self.records.entry(record.key.clone()) {
            Entry::Occupied(mut entry) => {
                entry.insert(record);
            }

            Entry::Vacant(entry) => {
                if len >= self.config.max_records {
                    return;
                }

                entry.insert(record);
            }
        }
    }

    /// Poll next event from the store.
    async fn next_event() -> Option<MemoryStoreEvent> {
        None
    }
}

pub struct MemoryStoreConfig {
    /// Maximum number of records to store.
    pub max_records: usize,

    /// Maximum size of a record in bytes.
    pub max_record_size_bytes: usize,
}

impl Default for MemoryStoreConfig {
    fn default() -> Self {
        Self {
            max_records: 1024,
            max_record_size_bytes: 1024,
        }
    }
}
