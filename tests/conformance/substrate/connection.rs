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

use litep2p::{
    config::ConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::notification::{handle::NotificationHandle, types::Config as NotificationConfig},
    transport::tcp::config::Config as TcpConfig,
    types::protocol::ProtocolName as Litep2pProtocol,
    Litep2p, Litep2pEvent,
};

use futures::StreamExt;
use libp2p::{
    identity,
    swarm::{SwarmBuilder, SwarmEvent},
    PeerId, Swarm,
};
use sc_network::{
    peer_store::{PeerStore, PeerStoreHandle},
    protocol::notifications::behaviour::{Notifications, ProtocolConfig},
    protocol_controller::{ProtoSetConfig, ProtocolController, SetId},
    types::ProtocolName,
};
use sc_utils::mpsc::tracing_unbounded;

use std::collections::HashSet;

fn initialize_libp2p(in_peers: u32, out_peers: u32) -> (Swarm<Notifications>, PeerStoreHandle) {
    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    let peer_store = PeerStore::new(vec![]);

    let (tx, rx) = tracing_unbounded("channel", 10_000);
    let proto_set_config = ProtoSetConfig {
        in_peers,
        out_peers,
        reserved_nodes: HashSet::new(),
        reserved_only: false,
    };

    let (handle, controller) = ProtocolController::new(
        SetId::from(0usize),
        proto_set_config,
        tx.clone(),
        Box::new(peer_store.handle()),
    );
    let peer_store_handle = peer_store.handle();
    tokio::spawn(controller.run());
    tokio::spawn(peer_store.run());

    let proto_config = ProtocolConfig {
        name: ProtocolName::from("/notif/1"),
        fallback_names: vec![],
        handshake: vec![1, 3, 3, 7],
        max_notification_size: 1000u64,
    };
    let behaviour = Notifications::new(vec![handle], rx, vec![proto_config].into_iter());
    let transport = libp2p::tokio_development_transport(local_key).unwrap();
    let mut swarm = SwarmBuilder::with_tokio_executor(transport, behaviour, local_peer_id).build();

    swarm.listen_on("/ip6/::1/tcp/0".parse().unwrap()).unwrap();

    (swarm, peer_store_handle)
}

async fn initialize_litep2p() -> (Litep2p, NotificationHandle) {
    let (notif_config1, handle) = NotificationConfig::new(
        Litep2pProtocol::from("/notif/1"),
        1024usize,
        vec![1, 3, 3, 8],
        Vec::new(),
    );
    let config1 = ConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_notification_protocol(notif_config1)
        .build();
    let litep2p = Litep2p::new(config1).await.unwrap();

    (litep2p, handle)
}

#[tokio::test]
async fn substrate_keep_alive_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, _peer_store_handle) = initialize_libp2p(1u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    let mut libp2p_connection_open = false;
    let mut libp2p_connection_closed = false;
    let mut litep2p_connection_open = false;
    let mut litep2p_connection_closed = false;

    while !libp2p_connection_open
        || !libp2p_connection_closed
        || !litep2p_connection_open
        || !litep2p_connection_closed
    {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { .. } => {
                    libp2p_connection_open = true;
                }
                SwarmEvent::ConnectionClosed { .. } => {
                    libp2p_connection_closed = true;
                }
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { .. } => {
                    litep2p_connection_open = true;
                }
                Litep2pEvent::ConnectionClosed { .. } => {
                    litep2p_connection_closed = true;
                }
                _ => {}
            },
            event = handle.next() => match event.unwrap() {
                event => tracing::debug!("unhanled notification event: {event:?}"),
            }
        }
    }
}
