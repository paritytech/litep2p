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
    identify, identity, kad,
    swarm::{keep_alive, NetworkBehaviour, SwarmBuilder, SwarmEvent},
    PeerId, Swarm,
};
use litep2p::{
    config::Litep2pConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::libp2p::kademlia::{ConfigBuilder, KademliaEvent, KademliaHandle},
    transport::tcp::config::TransportConfig as TcpTransportConfig,
    Litep2p,
};

#[derive(NetworkBehaviour)]
struct Behaviour {
    keep_alive: keep_alive::Behaviour,
    kad: kad::Kademlia<kad::store::MemoryStore>,
    identify: identify::Behaviour,
}

// initialize litep2p with ping support
async fn initialize_litep2p() -> (Litep2p, KademliaHandle) {
    let keypair = Keypair::generate();
    let (kad_config, kad_handle) = ConfigBuilder::new().build();

    let litep2p = Litep2p::new(
        Litep2pConfigBuilder::new()
            .with_keypair(keypair)
            .with_tcp(TcpTransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
                yamux_config: Default::default(),
            })
            .with_ipfs_kademlia(kad_config)
            .build(),
    )
    .await
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
            // ping: Default::default(),
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
async fn libp2p_dials() {
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
    let (mut litep2p, mut kad_handle) = initialize_litep2p().await;
    let address = litep2p.listen_addresses().next().unwrap().clone();

    for i in 0..addresses.len() {
        libp2p.dial(addresses[i].clone()).unwrap();
        let _ = libp2p
            .behaviour_mut()
            .kad
            .add_address(&peer_ids[i], addresses[i].clone());
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

    kad_handle
        .add_known_peer(
            litep2p::peer_id::PeerId::from_bytes(&peer_id.to_bytes()).unwrap(),
            vec![listen_addr.unwrap()],
        )
        .await;

    let target = litep2p::peer_id::PeerId::random();
    kad_handle.find_node(target).await;

    loop {
        match kad_handle.next().await {
            Some(KademliaEvent::FindNodeResult {
                target: query_target,
                peers,
            }) => {
                assert_eq!(target, query_target);
                assert!(!peers.is_empty());
                break;
            }
            _ => {}
        }
    }
}
