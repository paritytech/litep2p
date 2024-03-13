// Copyright 2018 Parity Technologies (UK) Ltd.
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

use futures::StreamExt;
use libp2p::{
    identify, identity,
    kad::{self, store::RecordStore},
    swarm::{keep_alive, NetworkBehaviour, SwarmBuilder, SwarmEvent},
    PeerId, Swarm,
};
use litep2p::{
    config::ConfigBuilder as Litep2pConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::libp2p::kademlia::{
        ConfigBuilder, KademliaEvent, KademliaHandle, Quorum, Record, RecordKey,
    },
    transport::tcp::config::Config as TcpConfig,
    Litep2p,
};
use multiaddr::Protocol;

#[derive(NetworkBehaviour)]
struct Behaviour {
    keep_alive: keep_alive::Behaviour,
    kad: kad::Kademlia<kad::store::MemoryStore>,
    identify: identify::Behaviour,
}

// initialize litep2p with ping support
fn initialize_litep2p() -> (Litep2p, KademliaHandle) {
    let keypair = Keypair::generate();
    let (kad_config, kad_handle) = ConfigBuilder::new().build();

    let litep2p = Litep2p::new(
        Litep2pConfigBuilder::new()
            .with_keypair(keypair)
            .with_tcp(TcpConfig {
                listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
                ..Default::default()
            })
            .with_libp2p_kademlia(kad_config)
            .build(),
    )
    .unwrap();

    (litep2p, kad_handle)
}

fn initialize_libp2p() -> Swarm<Behaviour> {
    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());

    tracing::debug!("Local peer id: {local_peer_id:?}");

    let transport = libp2p::tokio_development_transport(local_key.clone()).unwrap();
    let behaviour = {
        let config = kad::KademliaConfig::default();
        let store = kad::store::MemoryStore::new(local_peer_id);

        Behaviour {
            kad: kad::Kademlia::with_config(local_peer_id, store, config),
            keep_alive: Default::default(),
            identify: identify::Behaviour::new(identify::Config::new(
                "/ipfs/1.0.0".into(),
                local_key.public(),
            )),
        }
    };
    let mut swarm = SwarmBuilder::with_tokio_executor(transport, behaviour, local_peer_id).build();

    swarm.listen_on("/ip6/::1/tcp/0".parse().unwrap()).unwrap();

    swarm
}

#[tokio::test]
async fn find_node() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let mut addresses = vec![];
    let mut peer_ids = vec![];
    for _ in 0..3 {
        let mut libp2p = initialize_libp2p();

        loop {
            if let SwarmEvent::NewListenAddr { address, .. } = libp2p.select_next_some().await {
                addresses.push(address);
                peer_ids.push(*libp2p.local_peer_id());
                break;
            }
        }

        tokio::spawn(async move {
            loop {
                let _ = libp2p.select_next_some().await;
            }
        });
    }

    let mut libp2p = initialize_libp2p();
    let (mut litep2p, mut kad_handle) = initialize_litep2p();
    let address = litep2p.listen_addresses().next().unwrap().clone();

    for i in 0..addresses.len() {
        libp2p.dial(addresses[i].clone()).unwrap();
        let _ = libp2p.behaviour_mut().kad.add_address(&peer_ids[i], addresses[i].clone());
    }
    libp2p.dial(address).unwrap();

    tokio::spawn(async move {
        loop {
            let _ = litep2p.next_event().await;
        }
    });

    #[allow(unused)]
    let mut listen_addr = None;
    let peer_id = *libp2p.local_peer_id();

    tracing::error!("local peer id: {peer_id}");

    loop {
        if let SwarmEvent::NewListenAddr { address, .. } = libp2p.select_next_some().await {
            listen_addr = Some(address);
            break;
        }
    }

    tokio::spawn(async move {
        loop {
            let _ = libp2p.select_next_some().await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let listen_addr = listen_addr.unwrap().with(Protocol::P2p(peer_id.into()));

    kad_handle
        .add_known_peer(
            litep2p::PeerId::from_bytes(&peer_id.to_bytes()).unwrap(),
            vec![listen_addr],
        )
        .await;

    let target = litep2p::PeerId::random();
    let _ = kad_handle.find_node(target).await;

    loop {
        if let Some(KademliaEvent::FindNodeSuccess {
                target: query_target,
                peers,
                ..
            }) = kad_handle.next().await {
            assert_eq!(target, query_target);
            assert!(!peers.is_empty());
            break;
        }
    }
}

#[tokio::test]
async fn put_record() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let mut addresses = vec![];
    let mut peer_ids = vec![];
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0usize));

    for _ in 0..3 {
        let mut libp2p = initialize_libp2p();

        loop {
            if let SwarmEvent::NewListenAddr { address, .. } = libp2p.select_next_some().await {
                addresses.push(address);
                peer_ids.push(*libp2p.local_peer_id());
                break;
            }
        }

        let counter_copy = std::sync::Arc::clone(&counter);
        tokio::spawn(async move {
            let mut record_found = false;

            loop {
                tokio::select! {
                    _ = libp2p.select_next_some() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        let store = libp2p.behaviour_mut().kad.store_mut();
                        if store.get(&libp2p::kad::record::Key::new(&vec![1, 2, 3, 4])).is_some() && !record_found {
                            counter_copy.fetch_add(1usize, std::sync::atomic::Ordering::SeqCst);
                            record_found = true;
                        }
                    }
                }
            }
        });
    }

    let mut libp2p = initialize_libp2p();
    let (mut litep2p, mut kad_handle) = initialize_litep2p();
    let address = litep2p.listen_addresses().next().unwrap().clone();

    for i in 0..addresses.len() {
        libp2p.dial(addresses[i].clone()).unwrap();
        let _ = libp2p.behaviour_mut().kad.add_address(&peer_ids[i], addresses[i].clone());
    }
    libp2p.dial(address).unwrap();

    tokio::spawn(async move {
        loop {
            let _ = litep2p.next_event().await;
        }
    });

    #[allow(unused)]
    let mut listen_addr = None;
    let peer_id = *libp2p.local_peer_id();

    tracing::error!("local peer id: {peer_id}");

    loop {
        if let SwarmEvent::NewListenAddr { address, .. } = libp2p.select_next_some().await {
            listen_addr = Some(address);
            break;
        }
    }

    let counter_copy = std::sync::Arc::clone(&counter);
    tokio::spawn(async move {
        let mut record_found = false;

        loop {
            tokio::select! {
                _ = libp2p.select_next_some() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                    let store = libp2p.behaviour_mut().kad.store_mut();
                    if store.get(&libp2p::kad::record::Key::new(&vec![1, 2, 3, 4])).is_some() && !record_found {
                        counter_copy.fetch_add(1usize, std::sync::atomic::Ordering::SeqCst);
                        record_found = true;
                    }
                }
            }
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let listen_addr = listen_addr.unwrap().with(Protocol::P2p(peer_id.into()));

    kad_handle
        .add_known_peer(
            litep2p::PeerId::from_bytes(&peer_id.to_bytes()).unwrap(),
            vec![listen_addr],
        )
        .await;

    let record_key = RecordKey::new(&vec![1, 2, 3, 4]);
    let record = Record::new(record_key, vec![1, 3, 3, 7, 1, 3, 3, 8]);

    let _ = kad_handle.put_record(record).await;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        if counter.load(std::sync::atomic::Ordering::SeqCst) == 4 {
            break;
        }
    }
}

#[tokio::test]
async fn get_record() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let mut addresses = vec![];
    let mut peer_ids = vec![];
    let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0usize));

    for _ in 0..3 {
        let mut libp2p = initialize_libp2p();

        loop {
            if let SwarmEvent::NewListenAddr { address, .. } = libp2p.select_next_some().await {
                addresses.push(address);
                peer_ids.push(*libp2p.local_peer_id());
                break;
            }
        }

        let counter_copy = std::sync::Arc::clone(&counter);
        tokio::spawn(async move {
            let mut record_found = false;

            loop {
                tokio::select! {
                    _ = libp2p.select_next_some() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                        let store = libp2p.behaviour_mut().kad.store_mut();
                        if store.get(&libp2p::kad::record::Key::new(&vec![1, 2, 3, 4])).is_some() && !record_found {
                            counter_copy.fetch_add(1usize, std::sync::atomic::Ordering::SeqCst);
                            record_found = true;
                        }
                    }
                }
            }
        });
    }

    let mut libp2p = initialize_libp2p();
    let (mut litep2p, mut kad_handle) = initialize_litep2p();
    let address = litep2p.listen_addresses().next().unwrap().clone();

    for i in 0..addresses.len() {
        libp2p.dial(addresses[i].clone()).unwrap();
        let _ = libp2p.behaviour_mut().kad.add_address(&peer_ids[i], addresses[i].clone());
    }

    // publish record on the network
    let record = libp2p::kad::Record {
        key: libp2p::kad::RecordKey::new(&vec![1, 2, 3, 4]),
        value: vec![13, 37, 13, 38],
        publisher: None,
        expires: None,
    };
    libp2p.behaviour_mut().kad.put_record(record, libp2p::kad::Quorum::All).unwrap();

    #[allow(unused)]
    let mut listen_addr = None;

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => if let SwarmEvent::NewListenAddr { address, .. } = event {
                listen_addr = Some(address);
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(1)) => {
                if counter.load(std::sync::atomic::Ordering::SeqCst) == 3 {
                    break;
                }
            }
        }
    }

    libp2p.dial(address).unwrap();

    tokio::spawn(async move {
        loop {
            let _ = litep2p.next_event().await;
        }
    });

    let peer_id = *libp2p.local_peer_id();

    tokio::spawn(async move {
        loop {
            let _ = libp2p.select_next_some().await;
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    let listen_addr = listen_addr.unwrap().with(Protocol::P2p(peer_id.into()));

    kad_handle
        .add_known_peer(
            litep2p::PeerId::from_bytes(&peer_id.to_bytes()).unwrap(),
            vec![listen_addr],
        )
        .await;

    let _ = kad_handle.get_record(RecordKey::new(&vec![1, 2, 3, 4]), Quorum::All).await;

    loop {
        match kad_handle.next().await.unwrap() {
            KademliaEvent::GetRecordSuccess { .. } => break,
            KademliaEvent::RoutingTableUpdate { .. } => {}
            _ => panic!("invalid event received"),
        }
    }
}
