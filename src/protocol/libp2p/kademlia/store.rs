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

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::store";

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
    pub fn get(&mut self, key: &Key) -> Option<&Record> {
        let is_expired = self
            .records
            .get(key)
            .map_or(false, |record| record.is_expired(std::time::Instant::now()));

        if is_expired {
            self.records.remove(key);
            None
        } else {
            self.records.get(key)
        }
    }

    /// Store record.
    pub fn put(&mut self, record: Record) {
        if record.value.len() >= self.config.max_record_size_bytes {
            tracing::warn!(
                target: LOG_TARGET,
                key = ?record.key,
                publisher = ?record.publisher,
                size = record.value.len(),
                max_size = self.config.max_record_size_bytes,
                "discarding a DHT record that exceeds the configured size limit",
            );
            return;
        }

        let len = self.records.len();
        match self.records.entry(record.key.clone()) {
            Entry::Occupied(mut entry) => {
                // Lean towards the new record.
                if let (Some(stored_record_ttl), Some(new_record_ttl)) =
                    (entry.get().expires, record.expires)
                {
                    if stored_record_ttl > new_record_ttl {
                        return;
                    }
                }

                entry.insert(record);
            }

            Entry::Vacant(entry) => {
                if len >= self.config.max_records {
                    tracing::warn!(
                        target: LOG_TARGET,
                        max_records = self.config.max_records,
                        "discarding a DHT record, because maximum memory store size reached",
                    );
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
            max_record_size_bytes: 65 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_memory_store() {
        let mut store = MemoryStore::new();
        let key = Key::from(vec![1, 2, 3]);
        let record = Record::new(key.clone(), vec![4, 5, 6]);

        store.put(record.clone());
        assert_eq!(store.get(&key), Some(&record));
    }

    #[test]
    fn test_memory_store_length() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_records: 1,
            max_record_size_bytes: 1024,
        });

        let key1 = Key::from(vec![1, 2, 3]);
        let key2 = Key::from(vec![4, 5, 6]);
        let record1 = Record::new(key1.clone(), vec![4, 5, 6]);
        let record2 = Record::new(key2.clone(), vec![7, 8, 9]);

        store.put(record1.clone());
        store.put(record2.clone());

        assert_eq!(store.get(&key1), Some(&record1));
        assert_eq!(store.get(&key2), None);
    }

    #[test]
    fn test_memory_store_remove_old_records() {
        let mut store = MemoryStore::new();
        let key = Key::from(vec![1, 2, 3]);
        let record = Record {
            key: key.clone(),
            value: vec![4, 5, 6],
            publisher: None,
            expires: Some(std::time::Instant::now() - std::time::Duration::from_secs(5)),
        };
        // Record is already expired.
        assert!(record.is_expired(std::time::Instant::now()));

        store.put(record.clone());
        assert_eq!(store.get(&key), None);
    }

    #[test]
    fn test_memory_store_replace_new_records() {
        let mut store = MemoryStore::new();
        let key = Key::from(vec![1, 2, 3]);
        let record1 = Record {
            key: key.clone(),
            value: vec![4, 5, 6],
            publisher: None,
            expires: Some(std::time::Instant::now() + std::time::Duration::from_secs(100)),
        };
        let record2 = Record {
            key: key.clone(),
            value: vec![4, 5, 6],
            publisher: None,
            expires: Some(std::time::Instant::now() + std::time::Duration::from_secs(1000)),
        };

        store.put(record1.clone());
        assert_eq!(store.get(&key), Some(&record1));

        store.put(record2.clone());
        assert_eq!(store.get(&key), Some(&record2));
    }

    #[test]
    fn test_memory_store_max_record_size() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_records: 1024,
            max_record_size_bytes: 2,
        });

        let key = Key::from(vec![1, 2, 3]);
        let record = Record::new(key.clone(), vec![4, 5]);
        store.put(record.clone());
        assert_eq!(store.get(&key), None);

        let record = Record::new(key.clone(), vec![4]);
        store.put(record.clone());
        assert_eq!(store.get(&key), Some(&record));
    }
}
