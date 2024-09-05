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
use crate::protocol::libp2p::kademlia::record::{Key, ProviderRecord, Record};

use std::{
    collections::{hash_map::Entry, HashMap},
    num::NonZeroUsize,
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::kademlia::store";

/// Memory store events.
pub enum MemoryStoreEvent {}

/// Memory store.
pub struct MemoryStore {
    /// Records.
    records: HashMap<Key, Record>,
    /// Provider records.
    provider_keys: HashMap<Key, Vec<ProviderRecord>>,
    /// Configuration.
    config: MemoryStoreConfig,
}

impl MemoryStore {
    /// Create new [`MemoryStore`].
    pub fn new() -> Self {
        Self {
            records: HashMap::new(),
            provider_keys: HashMap::new(),
            config: MemoryStoreConfig::default(),
        }
    }

    /// Create new [`MemoryStore`] with the provided configuration.
    pub fn with_config(config: MemoryStoreConfig) -> Self {
        Self {
            records: HashMap::new(),
            provider_keys: HashMap::new(),
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

    /// Try to get providers from local store for `key`.
    ///
    /// Returns a non-empty list of providers, if any.
    pub fn get_providers(&mut self, key: &Key) -> Vec<ProviderRecord> {
        let drop = self.provider_keys.get_mut(key).map_or(false, |providers| {
            let now = std::time::Instant::now();
            providers.retain(|p| !p.is_expired(now));

            providers.is_empty()
        });

        if drop {
            self.provider_keys.remove(key);

            Vec::default()
        } else {
            self.provider_keys.get(key).cloned().unwrap_or_else(Vec::default)
        }
    }

    /// Try to add a provider for `key`. If there are already `max_providers_per_key` for
    /// this `key`, the new provider is only inserted if its closer to `key` than
    /// the furthest already inserted provider. The furthest provider is then discarded.
    ///
    /// Returns `true` if the provider was added, `false` otherwise.
    pub fn put_provider(&mut self, provider_record: ProviderRecord) -> bool {
        // Make sure we have no more than `max_provider_addresses`.
        let provider_record = {
            let mut record = provider_record;
            record.addresses.truncate(self.config.max_provider_addresses);
            record
        };

        let can_insert_new_key = self.provider_keys.len() < self.config.max_provider_keys;

        match self.provider_keys.entry(provider_record.key.clone()) {
            Entry::Vacant(entry) =>
                if can_insert_new_key {
                    entry.insert(vec![provider_record]);

                    true
                } else {
                    tracing::warn!(
                        target: LOG_TARGET,
                        max_provider_keys = self.config.max_provider_keys,
                        "discarding a provider record, because the provider key limit reached",
                    );

                    false
                },
            Entry::Occupied(mut entry) => {
                let mut providers = entry.get_mut();

                // Providers under every key are sorted by distance from the provided key, with
                // equal distances meaning peer IDs (more strictly, their hashes)
                // are equal.
                let provider_position =
                    providers.binary_search_by(|p| p.distance().cmp(&provider_record.distance()));

                match provider_position {
                    Ok(i) => {
                        // Update the provider in place.
                        providers[i] = provider_record;

                        true
                    }
                    Err(i) => {
                        // `Err(i)` contains the insertion point.
                        if i == self.config.max_providers_per_key {
                            tracing::trace!(
                                target: LOG_TARGET,
                                key = ?provider_record.key,
                                provider = ?provider_record.provider,
                                max_providers_per_key = self.config.max_providers_per_key,
                                "discarding a provider record, because it's further than \
                                 existing `max_providers_per_key`",
                            );

                            false
                        } else {
                            if providers.len() == self.config.max_providers_per_key {
                                providers.pop();
                            }

                            providers.insert(i, provider_record);

                            true
                        }
                    }
                }
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

    /// Maximum number of provider keys this node stores.
    pub max_provider_keys: usize,

    /// Maximum number of cached addresses per provider.
    pub max_provider_addresses: usize,

    /// Maximum number of providers per key. Only providers with peer IDs closest to the key are
    /// kept.
    pub max_providers_per_key: usize,
}

impl Default for MemoryStoreConfig {
    fn default() -> Self {
        Self {
            max_records: 1024,
            max_record_size_bytes: 65 * 1024,
            max_provider_keys: 1024,
            max_provider_addresses: 30,
            max_providers_per_key: 20,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PeerId;
    use multiaddr::{
        multiaddr,
        Protocol::{Ip4, Tcp},
    };

    #[test]
    fn put_get_record() {
        let mut store = MemoryStore::new();
        let key = Key::from(vec![1, 2, 3]);
        let record = Record::new(key.clone(), vec![4, 5, 6]);

        store.put(record.clone());
        assert_eq!(store.get(&key), Some(&record));
    }

    #[test]
    fn max_records() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_records: 1,
            max_record_size_bytes: 1024,
            ..Default::default()
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
    fn expired_record_removed() {
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
    fn new_record_overwrites() {
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
    fn max_record_size() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_records: 1024,
            max_record_size_bytes: 2,
            ..Default::default()
        });

        let key = Key::from(vec![1, 2, 3]);
        let record = Record::new(key.clone(), vec![4, 5]);
        store.put(record.clone());
        assert_eq!(store.get(&key), None);

        let record = Record::new(key.clone(), vec![4]);
        store.put(record.clone());
        assert_eq!(store.get(&key), Some(&record));
    }

    #[test]
    fn put_get_provider() {
        let mut store = MemoryStore::new();
        let provider = ProviderRecord {
            key: Key::from(vec![1, 2, 3]),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };

        store.put_provider(provider.clone());
        assert_eq!(store.get_providers(&provider.key), vec![provider]);
    }

    #[test]
    fn multiple_providers_per_key() {
        let mut store = MemoryStore::new();
        let key = Key::from(vec![1, 2, 3]);
        let provider1 = ProviderRecord {
            key: key.clone(),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };
        let provider2 = ProviderRecord {
            key: key.clone(),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };

        store.put_provider(provider1.clone());
        store.put_provider(provider2.clone());

        let got_providers = store.get_providers(&key);
        assert_eq!(got_providers.len(), 2);
        assert!(got_providers.contains(&provider1));
        assert!(got_providers.contains(&provider2));
    }

    #[test]
    fn providers_sorted_by_distance() {
        let mut store = MemoryStore::new();
        let key = Key::from(vec![1, 2, 3]);
        let providers = (0..10)
            .map(|_| ProviderRecord {
                key: key.clone(),
                provider: PeerId::random(),
                addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            })
            .collect::<Vec<_>>();

        providers.iter().for_each(|p| {
            store.put_provider(p.clone());
        });

        let sorted_providers = {
            let mut providers = providers;
            providers.sort_unstable_by_key(ProviderRecord::distance);
            providers
        };

        assert_eq!(store.get_providers(&key), sorted_providers);
    }

    #[test]
    fn max_providers_per_key() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_providers_per_key: 10,
            ..Default::default()
        });
        let key = Key::from(vec![1, 2, 3]);
        let providers = (0..20)
            .map(|_| ProviderRecord {
                key: key.clone(),
                provider: PeerId::random(),
                addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            })
            .collect::<Vec<_>>();

        providers.iter().for_each(|p| {
            store.put_provider(p.clone());
        });
        assert_eq!(store.get_providers(&key).len(), 10);
    }

    #[test]
    fn closest_providers_kept() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_providers_per_key: 10,
            ..Default::default()
        });
        let key = Key::from(vec![1, 2, 3]);
        let providers = (0..20)
            .map(|_| ProviderRecord {
                key: key.clone(),
                provider: PeerId::random(),
                addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            })
            .collect::<Vec<_>>();

        providers.iter().for_each(|p| {
            store.put_provider(p.clone());
        });

        let closest_providers = {
            let mut providers = providers;
            providers.sort_unstable_by_key(ProviderRecord::distance);
            providers.truncate(10);
            providers
        };

        assert_eq!(store.get_providers(&key), closest_providers);
    }

    #[test]
    fn furthest_provider_discarded() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_providers_per_key: 10,
            ..Default::default()
        });
        let key = Key::from(vec![1, 2, 3]);
        let providers = (0..11)
            .map(|_| ProviderRecord {
                key: key.clone(),
                provider: PeerId::random(),
                addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            })
            .collect::<Vec<_>>();

        let sorted_providers = {
            let mut providers = providers;
            providers.sort_unstable_by_key(ProviderRecord::distance);
            providers
        };

        // First 10 providers are inserted.
        for i in 0..10 {
            assert!(store.put_provider(sorted_providers[i].clone()));
        }
        assert_eq!(store.get_providers(&key), sorted_providers[..10]);

        // The furthests provider doesn't fit.
        assert!(!store.put_provider(sorted_providers[10].clone()));
        assert_eq!(store.get_providers(&key), sorted_providers[..10]);
    }

    #[test]
    fn update_provider_in_place() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_providers_per_key: 10,
            ..Default::default()
        });
        let key = Key::from(vec![1, 2, 3]);
        let peer_ids = (0..10).map(|_| PeerId::random()).collect::<Vec<_>>();
        let peer_id0 = peer_ids[0];
        let providers = peer_ids
            .iter()
            .map(|peer_id| ProviderRecord {
                key: key.clone(),
                provider: *peer_id,
                addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
                expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
            })
            .collect::<Vec<_>>();

        providers.iter().for_each(|p| {
            store.put_provider(p.clone());
        });

        let sorted_providers = {
            let mut providers = providers;
            providers.sort_unstable_by_key(ProviderRecord::distance);
            providers
        };

        assert_eq!(store.get_providers(&key), sorted_providers);

        let provider0_new = ProviderRecord {
            key: key.clone(),
            provider: peer_id0,
            addresses: vec![multiaddr!(Ip4([192, 168, 0, 1]), Tcp(20000u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };

        // Provider is updated in place.
        assert!(store.put_provider(provider0_new.clone()));

        let providers_new = sorted_providers
            .into_iter()
            .map(|p| {
                if p.provider == peer_id0 {
                    provider0_new.clone()
                } else {
                    p
                }
            })
            .collect::<Vec<_>>();

        assert_eq!(store.get_providers(&key), providers_new);
    }

    #[test]
    fn provider_record_expires() {
        let mut store = MemoryStore::new();
        let provider = ProviderRecord {
            key: Key::from(vec![1, 2, 3]),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
            expires: std::time::Instant::now() - std::time::Duration::from_secs(5),
        };

        // Provider record is already expired.
        assert!(provider.is_expired(std::time::Instant::now()));

        store.put_provider(provider.clone());
        assert!(store.get_providers(&provider.key).is_empty());
    }

    #[test]
    fn individual_provider_record_expires() {
        let mut store = MemoryStore::new();
        let key = Key::from(vec![1, 2, 3]);
        let provider1 = ProviderRecord {
            key: key.clone(),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
            expires: std::time::Instant::now() - std::time::Duration::from_secs(5),
        };
        let provider2 = ProviderRecord {
            key: key.clone(),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };

        assert!(provider1.is_expired(std::time::Instant::now()));

        store.put_provider(provider1.clone());
        store.put_provider(provider2.clone());

        assert_eq!(store.get_providers(&key), vec![provider2]);
    }

    #[test]
    fn max_addresses_per_provider() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_provider_addresses: 2,
            ..Default::default()
        });
        let key = Key::from(vec![1, 2, 3]);
        let provider = ProviderRecord {
            key: Key::from(vec![1, 2, 3]),
            provider: PeerId::random(),
            addresses: vec![
                multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10000u16)),
                multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10001u16)),
                multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10002u16)),
                multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10003u16)),
                multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10004u16)),
            ],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };

        store.put_provider(provider);

        let got_providers = store.get_providers(&key);
        assert_eq!(got_providers.len(), 1);
        assert_eq!(got_providers.first().unwrap().key, key);
        assert_eq!(got_providers.first().unwrap().addresses.len(), 2);
    }

    #[test]
    fn max_provider_keys() {
        let mut store = MemoryStore::with_config(MemoryStoreConfig {
            max_provider_keys: 2,
            ..Default::default()
        });

        let provider1 = ProviderRecord {
            key: Key::from(vec![1, 2, 3]),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10001u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };
        let provider2 = ProviderRecord {
            key: Key::from(vec![4, 5, 6]),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10002u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };
        let provider3 = ProviderRecord {
            key: Key::from(vec![7, 8, 9]),
            provider: PeerId::random(),
            addresses: vec![multiaddr!(Ip4([127, 0, 0, 1]), Tcp(10003u16))],
            expires: std::time::Instant::now() + std::time::Duration::from_secs(3600),
        };

        assert!(store.put_provider(provider1.clone()));
        assert!(store.put_provider(provider2.clone()));
        assert!(!store.put_provider(provider3.clone()));

        assert_eq!(store.get_providers(&provider1.key), vec![provider1]);
        assert_eq!(store.get_providers(&provider2.key), vec![provider2]);
        assert_eq!(store.get_providers(&provider3.key), vec![]);
    }
}
