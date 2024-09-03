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

#![allow(unused)]
use bytes::Bytes;
use futures::StreamExt;
use litep2p::{
    config::ConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::libp2p::kademlia::{
        ConfigBuilder as KademliaConfigBuilder, IncomingRecordValidationMode, KademliaEvent,
        PeerRecord, Quorum, Record, RecordKey, RecordsType,
    },
    transport::tcp::config::Config as TcpConfig,
    Litep2p, PeerId,
};

fn spawn_litep2p(port: u16) {
    let (kad_config1, _kad_handle1) = KademliaConfigBuilder::new().build();
    let config1 = ConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpConfig {
            listen_addresses: vec![format!("/ip6/::1/tcp/{port}").parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config1)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();

    tokio::spawn(async move { while let Some(_) = litep2p1.next_event().await {} });
}

#[tokio::test]
#[ignore]
async fn kademlia_supported() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (kad_config1, _kad_handle1) = KademliaConfigBuilder::new().build();
    let config1 = ConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config1)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();

    for port in 9000..9003 {
        spawn_litep2p(port);
    }

    loop {
        tokio::select! {
            event = litep2p1.next_event() => {
                tracing::info!("litep2p event received: {event:?}");
            }
            // event = kad_handle1.next() => {
            //     tracing::info!("kademlia event received: {event:?}");
            // }
        }
    }
}

#[tokio::test]
#[ignore]
async fn put_value() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (kad_config1, mut kad_handle1) = KademliaConfigBuilder::new().build();
    let config1 = ConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config1)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();

    for i in 0..10 {
        kad_handle1
            .add_known_peer(
                PeerId::random(),
                vec![format!("/ip6/::/tcp/{i}").parse().unwrap()],
            )
            .await;
    }

    // let key = RecordKey::new(&Bytes::from(vec![1, 3, 3, 7]));
    // kad_handle1.put_value(key, vec![1, 2, 3, 4]).await;

    // loop {
    //     tokio::select! {
    //         event = litep2p1.next_event() => {
    //             tracing::info!("litep2p event received: {event:?}");
    //         }
    //         event = kad_handle1.next() => {
    //             tracing::info!("kademlia event received: {event:?}");
    //         }
    //     }
    // }
}

#[tokio::test]
async fn records_are_stored_automatically() {
    let (kad_config1, mut kad_handle1) = KademliaConfigBuilder::new().build();
    let (kad_config2, mut kad_handle2) = KademliaConfigBuilder::new().build();

    let config1 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config1)
        .build();

    let config2 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();
    let mut litep2p2 = Litep2p::new(config2).unwrap();

    kad_handle1
        .add_known_peer(
            *litep2p2.local_peer_id(),
            litep2p2.listen_addresses().cloned().collect(),
        )
        .await;

    // Publish the record.
    let record = Record::new(vec![1, 2, 3], vec![0x01]);
    kad_handle1.put_record(record.clone()).await;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => {
                panic!("record was not stored in 10 secs")
            }
            _ = litep2p1.next_event() => {}
            _ = litep2p2.next_event() => {}
            _ = kad_handle1.next() => {}
            event = kad_handle2.next() => {
                match event {
                    Some(KademliaEvent::IncomingRecord { record: got_record }) => {
                        assert_eq!(got_record.key, record.key);
                        assert_eq!(got_record.value, record.value);
                        assert_eq!(got_record.publisher.unwrap(), *litep2p1.local_peer_id());
                        assert!(got_record.expires.is_some());

                        // Check if the record was stored.
                        let _ = kad_handle2
                            .get_record(RecordKey::from(vec![1, 2, 3]), Quorum::One).await;
                    }
                    Some(KademliaEvent::GetRecordSuccess { query_id: _, records }) => {
                        match records {
                            RecordsType::LocalStore(got_record) => {
                                assert_eq!(got_record.key, record.key);
                                assert_eq!(got_record.value, record.value);
                                assert_eq!(got_record.publisher.unwrap(), *litep2p1.local_peer_id());
                                assert!(got_record.expires.is_some());

                                break
                            }
                            RecordsType::Network(_) => {
                                panic!("record was not stored locally")
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn records_are_stored_manually() {
    let (kad_config1, mut kad_handle1) = KademliaConfigBuilder::new()
        .with_incoming_records_validation_mode(IncomingRecordValidationMode::Manual)
        .build();
    let (kad_config2, mut kad_handle2) = KademliaConfigBuilder::new()
        .with_incoming_records_validation_mode(IncomingRecordValidationMode::Manual)
        .build();

    let config1 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config1)
        .build();

    let config2 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();
    let mut litep2p2 = Litep2p::new(config2).unwrap();

    kad_handle1
        .add_known_peer(
            *litep2p2.local_peer_id(),
            litep2p2.listen_addresses().cloned().collect(),
        )
        .await;

    // Publish the record.
    let mut record = Record::new(vec![1, 2, 3], vec![0x01]);
    kad_handle1.put_record(record.clone()).await;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => {
                panic!("record was not stored in 10 secs")
            }
            _ = litep2p1.next_event() => {}
            _ = litep2p2.next_event() => {}
            _ = kad_handle1.next() => {}
            event = kad_handle2.next() => {
                match event {
                    Some(KademliaEvent::IncomingRecord { record: got_record }) => {
                        assert_eq!(got_record.key, record.key);
                        assert_eq!(got_record.value, record.value);
                        assert_eq!(got_record.publisher.unwrap(), *litep2p1.local_peer_id());
                        assert!(got_record.expires.is_some());

                        kad_handle2.store_record(got_record).await;

                        // Check if the record was stored.
                        let _ = kad_handle2
                            .get_record(RecordKey::from(vec![1, 2, 3]), Quorum::One).await;
                    }
                    Some(KademliaEvent::GetRecordSuccess { query_id: _, records }) => {
                        match records {
                            RecordsType::LocalStore(got_record) => {
                                assert_eq!(got_record.key, record.key);
                                assert_eq!(got_record.value, record.value);
                                assert_eq!(got_record.publisher.unwrap(), *litep2p1.local_peer_id());
                                assert!(got_record.expires.is_some());

                                break
                            }
                            RecordsType::Network(_) => {
                                panic!("record was not stored locally")
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn not_validated_records_are_not_stored() {
    let (kad_config1, mut kad_handle1) = KademliaConfigBuilder::new()
        .with_incoming_records_validation_mode(IncomingRecordValidationMode::Manual)
        .build();
    let (kad_config2, mut kad_handle2) = KademliaConfigBuilder::new()
        .with_incoming_records_validation_mode(IncomingRecordValidationMode::Manual)
        .build();

    let config1 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config1)
        .build();

    let config2 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();
    let mut litep2p2 = Litep2p::new(config2).unwrap();

    kad_handle1
        .add_known_peer(
            *litep2p2.local_peer_id(),
            litep2p2.listen_addresses().cloned().collect(),
        )
        .await;

    // Publish the record.
    let record = Record::new(vec![1, 2, 3], vec![0x01]);
    kad_handle1.put_record(record.clone()).await;

    let mut get_record_query_id = None;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => {
                panic!("query has not failed in 10 secs")
            }
            event = litep2p1.next_event() => {}
            event = litep2p2.next_event() => {}
            event = kad_handle1.next() => {}
            event = kad_handle2.next() => {
                match event {
                    Some(KademliaEvent::IncomingRecord { record: got_record }) => {
                        assert_eq!(got_record.key, record.key);
                        assert_eq!(got_record.value, record.value);
                        assert_eq!(got_record.publisher.unwrap(), *litep2p1.local_peer_id());
                        assert!(got_record.expires.is_some());
                        // Do not call `kad_handle2.store_record(record).await`.

                        // Check if the record was stored.
                        let query_id = kad_handle2
                            .get_record(RecordKey::from(vec![1, 2, 3]), Quorum::One).await;
                        get_record_query_id = Some(query_id);
                    }
                    Some(KademliaEvent::GetRecordSuccess { query_id: _, records }) => {
                        match records {
                            RecordsType::LocalStore(_) => {
                                panic!("the record was added without validation")
                            }
                            RecordsType::Network(_) => break
                        }
                    }
                    Some(KademliaEvent::QueryFailed { query_id }) => {
                        assert_eq!(query_id, get_record_query_id.unwrap());
                        break
                    }
                    _ => {}
                }
            }
        }
    }
}

#[tokio::test]
async fn get_record_retrieves_remote_records() {
    let (kad_config1, mut kad_handle1) = KademliaConfigBuilder::new()
        .with_incoming_records_validation_mode(IncomingRecordValidationMode::Manual)
        .build();
    let (kad_config2, mut kad_handle2) = KademliaConfigBuilder::new()
        .with_incoming_records_validation_mode(IncomingRecordValidationMode::Manual)
        .build();

    let config1 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config1)
        .build();

    let config2 = ConfigBuilder::new()
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_kademlia(kad_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();
    let mut litep2p2 = Litep2p::new(config2).unwrap();

    // Store the record on `litep2p1``.
    let original_record = Record::new(vec![1, 2, 3], vec![0x01]);
    let query1 = kad_handle1.put_record(original_record.clone()).await;

    let mut query2 = None;

    loop {
        tokio::select! {
            _ = tokio::time::sleep(tokio::time::Duration::from_secs(10)) => {
                panic!("record was not retrieved in 10 secs")
            }
            event = litep2p1.next_event() => {}
            event = litep2p2.next_event() => {}
            event = kad_handle1.next() => {
                match event {
                    Some(KademliaEvent::QueryFailed { query_id }) => {
                        // Query failed, but the record was stored locally.
                        assert_eq!(query_id, query1);

                        // Let peer2 know about peer1.
                        kad_handle2
                            .add_known_peer(
                                *litep2p1.local_peer_id(),
                                litep2p1.listen_addresses().cloned().collect(),
                            )
                            .await;

                        // Let peer2 get record from peer1.
                        let query_id = kad_handle2
                            .get_record(RecordKey::from(vec![1, 2, 3]), Quorum::One).await;
                        query2 = Some(query_id);
                    }
                    _ => {}
                }
            }
            event = kad_handle2.next() => {
                match event {
                    Some(KademliaEvent::GetRecordSuccess { query_id: _, records }) => {
                        match records {
                            RecordsType::LocalStore(_) => {
                                panic!("the record was unexpectedly added to peer2")
                            }
                            RecordsType::Network(records) => {
                                assert_eq!(records.len(), 1);

                                let PeerRecord { peer, record } = records.first().unwrap();
                                assert_eq!(peer, litep2p1.local_peer_id());
                                assert_eq!(record.key, original_record.key);
                                assert_eq!(record.value, original_record.value);
                                assert_eq!(record.publisher.unwrap(), *litep2p1.local_peer_id());
                                assert!(record.expires.is_some());

                                break
                            }
                        }
                    }
                    Some(KademliaEvent::QueryFailed { query_id: _ }) => {
                        panic!("query failed")
                    }
                    _ => {}
                }
            }
        }
    }
}
