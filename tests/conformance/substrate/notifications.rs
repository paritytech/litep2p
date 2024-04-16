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
    protocol::notification::{
        handle::NotificationHandle,
        types::{
            Config as NotificationConfig, NotificationError, NotificationEvent, ValidationResult,
        },
    },
    transport::tcp::config::Config as TcpConfig,
    types::protocol::ProtocolName as Litep2pProtocol,
    Litep2p, Litep2pEvent, PeerId as Litep2pPeerId,
};

use futures::StreamExt;
use libp2p::{
    identity,
    swarm::{SwarmBuilder, SwarmEvent},
    PeerId, Swarm,
};
use sc_network::{
    peer_store::{PeerStore, PeerStoreHandle, PeerStoreProvider},
    protocol::notifications::behaviour::{Notifications, NotificationsOut, ProtocolConfig},
    protocol_controller::{ProtoSetConfig, ProtocolController, SetId},
    types::ProtocolName,
    ReputationChange,
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
async fn substrate_open_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, mut peer_store_handle) = initialize_libp2p(1u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let libp2p_peer = *libp2p.local_peer_id();
    let litep2p_peer = *litep2p.local_peer_id();

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    let mut libp2p_ready = false;
    let mut litep2p_ready = false;
    let mut litep2p_3333_seen = false;
    let mut litep2p_4444_seen = false;
    let mut libp2p_1111_seen = false;
    let mut libp2p_2222_seen = false;

    while !libp2p_ready
        || !litep2p_ready
        || !litep2p_3333_seen
        || !litep2p_4444_seen
        || !libp2p_1111_seen
        || !libp2p_2222_seen
    {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { .. } => {
                    peer_store_handle.add_known_peer(PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap());
                }
                SwarmEvent::Behaviour(NotificationsOut::CustomProtocolOpen {
                    peer_id, set_id, negotiated_fallback, received_handshake, notifications_sink, inbound,
                }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));
                    assert_eq!(received_handshake, vec![1, 3, 3, 8]);
                    assert!(negotiated_fallback.is_none());
                    assert!(!inbound);

                    notifications_sink.reserve_notification().await.unwrap().send(vec![3, 3, 3, 3]).unwrap();
                    notifications_sink.send_sync_notification(vec![4, 4, 4, 4]);

                    libp2p_ready = true;
                }
                SwarmEvent::Behaviour(NotificationsOut::Notification { peer_id, set_id, message }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));

                    if message == vec![1, 1, 1, 1] {
                        libp2p_1111_seen = true;
                    } else if message == vec![2, 2, 2, 2] {
                        libp2p_2222_seen = true;
                    }
                }
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event {
                event => tracing::info!("unhanled litep2p event: {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_validation_result(peer, ValidationResult::Accept).await;
                    litep2p_ready = true;
                }
                NotificationEvent::NotificationStreamOpened { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_sync_notification(peer, vec![1, 1, 1, 1]).unwrap();
                    handle.send_async_notification(peer, vec![2, 2, 2, 2]).await.unwrap();
                }
                NotificationEvent::NotificationReceived { peer, notification } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());

                    if notification == vec![3, 3, 3, 3] {
                        litep2p_3333_seen = true;
                    } else if notification == vec![4, 4, 4, 4] {
                        litep2p_4444_seen = true;
                    }
                }
                event => tracing::error!("unhanled notification event: {event:?}"),
            }
        }
    }
}

#[tokio::test]
async fn litep2p_open_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, _peer_store_handle) = initialize_libp2p(1u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let libp2p_peer = *libp2p.local_peer_id();
    let litep2p_peer = *litep2p.local_peer_id();

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    let mut libp2p_ready = false;
    let mut litep2p_ready = false;
    let mut litep2p_3333_seen = false;
    let mut litep2p_4444_seen = false;
    let mut libp2p_1111_seen = false;
    let mut libp2p_2222_seen = false;

    while !libp2p_ready
        || !litep2p_ready
        || !litep2p_3333_seen
        || !litep2p_4444_seen
        || !libp2p_1111_seen
        || !libp2p_2222_seen
    {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                SwarmEvent::Behaviour(NotificationsOut::CustomProtocolOpen {
                    peer_id, set_id, negotiated_fallback, received_handshake, notifications_sink, inbound,
                }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));
                    assert_eq!(received_handshake, vec![1, 3, 3, 8]);
                    assert!(negotiated_fallback.is_none());
                    assert!(inbound);

                    notifications_sink.reserve_notification().await.unwrap().send(vec![3, 3, 3, 3]).unwrap();
                    notifications_sink.send_sync_notification(vec![4, 4, 4, 4]);

                    libp2p_ready = true;
                }
                SwarmEvent::Behaviour(NotificationsOut::Notification { peer_id, set_id, message }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));

                    if message == vec![1, 1, 1, 1] {
                        libp2p_1111_seen = true;
                    } else if message == vec![2, 2, 2, 2] {
                        libp2p_2222_seen = true;
                    }
                }
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { peer, .. } => {
                    // TODO: zzz
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    handle.open_substream(peer).await.unwrap();
                }
                event => tracing::info!("unhanled litep2p event: {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_validation_result(peer, ValidationResult::Accept).await;
                    litep2p_ready = true;
                }
                NotificationEvent::NotificationStreamOpened { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_sync_notification(peer, vec![1, 1, 1, 1]).unwrap();
                    handle.send_async_notification(peer, vec![2, 2, 2, 2]).await.unwrap();
                }
                NotificationEvent::NotificationReceived { peer, notification } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());

                    if notification == vec![3, 3, 3, 3] {
                        litep2p_3333_seen = true;
                    } else if notification == vec![4, 4, 4, 4] {
                        litep2p_4444_seen = true;
                    }
                }
                event => tracing::error!("unhanled notification event: {event:?}"),
            }
        }
    }
}

#[tokio::test]
async fn substrate_reject_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    // set inbound peer to count 0 so `ProtocolController` will reject the peer
    let (mut libp2p, _peer_store_handle) = initialize_libp2p(0u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let libp2p_peer = *libp2p.local_peer_id();

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { peer, .. } => {
                    // TODO: zzz
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    handle.open_substream(peer).await.unwrap();
                }
                event => tracing::info!("unhanled litep2p event: {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                NotificationEvent::NotificationStreamOpenFailure { peer, error } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(error, NotificationError::Rejected);
                    break;
                }
                NotificationEvent::NotificationStreamClosed { .. } => break,
                event => tracing::error!("unhanled notification event: {event:?}"),
            }
        }
    }
}

// NOTE: there is a known bug in Substrate where `ProtocolController` opens a connection to the peer
// right after it has been rejected it:
//    - https://github.com/paritytech/substrate/issues/13778#issuecomment-1684925132
// This can cause this test to hang be as `litep2p` thinks it still connected to the peer while in
// fact the peer has disconnected
//
// This needs to fixed in Substrate.
#[tokio::test]
#[ignore]
async fn litep2p_reject_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, mut peer_store_handle) = initialize_libp2p(1u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let libp2p_peer = *libp2p.local_peer_id();
    let litep2p_peer = *litep2p.local_peer_id();

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { .. } => {
                    peer_store_handle.add_known_peer(PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap());
                }
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event {
                Some(Litep2pEvent::ConnectionClosed { .. }) => break,
                event => tracing::info!("unhanled litep2p event: {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_validation_result(peer, ValidationResult::Reject).await;

                    tracing::info!("reject substream");
                }
                event => tracing::error!("unhanled notification event: {event:?}"),
            }
        }
    }
}

// NOTE: there is a known bug in Substrate where `ProtocolController` opens a connection to the peer
// right after it has disconnected it:
//    - https://github.com/paritytech/substrate/issues/13778#issuecomment-1684925132
// This can cause this test to fail be as `litep2p` thinks it still connected to the peer while in
// fact the peer has disconnected
//
// TODO: this might have to be fixed on `litep2p` side because in practice nothing should prevent a
// node from disconnecting and reconning right after.
#[tokio::test]
#[ignore]
async fn substrate_close_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, mut peer_store_handle) = initialize_libp2p(1u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let libp2p_peer = *libp2p.local_peer_id();
    let litep2p_peer = *litep2p.local_peer_id();

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    let mut libp2p_notification_count = 0;

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { .. } => {
                    peer_store_handle.add_known_peer(PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap());
                }
                SwarmEvent::Behaviour(NotificationsOut::CustomProtocolOpen {
                    peer_id, set_id, negotiated_fallback, received_handshake, notifications_sink, inbound,
                }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));
                    assert_eq!(received_handshake, vec![1, 3, 3, 8]);
                    assert!(negotiated_fallback.is_none());
                    assert!(!inbound);

                    notifications_sink.reserve_notification().await.unwrap().send(vec![3, 3, 3, 3]).unwrap();
                    notifications_sink.send_sync_notification(vec![4, 4, 4, 4]);
                }
                SwarmEvent::Behaviour(NotificationsOut::Notification { peer_id, set_id, .. }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));

                   if libp2p_notification_count == 0 {
                        libp2p_notification_count += 1;
                    } else {
                        libp2p_notification_count += 1;
                        libp2p.behaviour_mut().disconnect_peer(&peer_id, set_id);
                    }
                }
                SwarmEvent::Behaviour(NotificationsOut::CustomProtocolClosed { .. }) => {
                    handle.send_sync_notification(
                        Litep2pPeerId::from_bytes(&libp2p_peer.to_bytes()).unwrap(),
                        vec![1 ,2 , 3, 4]
                    ).unwrap();
                }
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event.unwrap() {
                event => tracing::info!("unhanled litep2p event: {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_validation_result(peer, ValidationResult::Accept).await;
                }
                NotificationEvent::NotificationStreamOpened { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_sync_notification(peer, vec![1, 1, 1, 1]).unwrap();
                    handle.send_async_notification(peer, vec![2, 2, 2, 2]).await.unwrap();
                }
                NotificationEvent::NotificationReceived { peer, .. } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                }
                NotificationEvent::NotificationStreamClosed { peer } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    break;
                }
                event => tracing::error!("unhanled notification event: {event:?}"),
            }
        }
    }
}

// NOTE: Substrate doesn't consider the inbound substream closed as error which would disconnect
// the peer. This is a known bug/annoyance which should be fixed and the protocol should be
// informed if either substream is closed.
//
// This test recreates the current behavior in Substrate whereby the closed connection is detected
// only when the protocol tries to write something to the substream.
#[tokio::test]
async fn litep2p_close_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, mut peer_store_handle) = initialize_libp2p(1u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let libp2p_peer = *libp2p.local_peer_id();
    let litep2p_peer = *litep2p.local_peer_id();

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    let mut notif_count = 0;
    let mut peerse = None;

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { .. } => {
                    peer_store_handle.add_known_peer(PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap());
                }
                SwarmEvent::Behaviour(NotificationsOut::CustomProtocolOpen {
                    peer_id, set_id, negotiated_fallback, received_handshake, inbound, ..
                }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));
                    assert_eq!(received_handshake, vec![1, 3, 3, 8]);
                    assert!(negotiated_fallback.is_none());
                    assert!(!inbound);
                }
                SwarmEvent::Behaviour(NotificationsOut::Notification { peer_id, set_id, .. }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));

                    notif_count += 1;
                    if notif_count == 2 {
                        handle.close_substream(peerse.unwrap()).await;
                    }
                }
                SwarmEvent::Behaviour(NotificationsOut::CustomProtocolClosed { .. }) => {
                    break;
                }
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event.unwrap() {
                event => tracing::info!("unhanled litep2p event: {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_validation_result(peer, ValidationResult::Accept).await;
                }
                NotificationEvent::NotificationStreamOpened { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_sync_notification(peer, vec![1, 1, 1, 1]).unwrap();
                    handle.send_async_notification(peer, vec![2, 2, 2, 2]).await.unwrap();
                    peerse = Some(peer);
                }
                NotificationEvent::NotificationReceived { peer, .. } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    notif_count += 1;
                }
                NotificationEvent::NotificationStreamClosed { .. } => {
                    break;
                }
                event => tracing::error!("unhanled notification event: {event:?}"),
            },
        }
    }
}

#[tokio::test]
async fn both_nodes_open_substreams() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, mut peer_store_handle) = initialize_libp2p(1u32, 1u32);
    let (mut litep2p, mut handle) = initialize_litep2p().await;

    let libp2p_peer = *libp2p.local_peer_id();
    let litep2p_peer = *litep2p.local_peer_id();

    let address = litep2p.listen_addresses().next().unwrap().clone();
    libp2p.dial(address).unwrap();

    let mut libp2p_ready = false;
    let mut litep2p_ready = false;

    while !litep2p_ready || !libp2p_ready {
        tokio::select! {
            event = libp2p.select_next_some() => match event {
                SwarmEvent::ConnectionEstablished { .. } => {
                    peer_store_handle.add_known_peer(PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap());
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    handle.open_substream(Litep2pPeerId::from_bytes(&libp2p_peer.to_bytes()).unwrap()).await.unwrap();
                }
                SwarmEvent::Behaviour(NotificationsOut::CustomProtocolOpen {
                    peer_id, set_id, negotiated_fallback, received_handshake, ..
                }) => {
                    assert_eq!(peer_id.to_bytes(), litep2p_peer.to_bytes());
                    assert_eq!(set_id, SetId::from(0usize));
                    assert_eq!(received_handshake, vec![1, 3, 3, 8]);
                    assert!(negotiated_fallback.is_none());

                    libp2p_ready = true;
                }
                event => tracing::info!("unhanled libp2p event: {event:?}"),
            },
            event = litep2p.next_event() => match event.unwrap() {
                event => tracing::info!("unhanled litep2p event: {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    handle.send_validation_result(peer, ValidationResult::Accept).await;
                    litep2p_ready = true;
                }
                NotificationEvent::NotificationStreamOpened { protocol, peer, handshake } => {
                    assert_eq!(protocol, Litep2pProtocol::from("/notif/1"));
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(handshake, vec![1, 3, 3, 7]);

                    litep2p_ready = true;
                }
                event => tracing::error!("unhanled notification event: {event:?}"),
            }
        }
    }
}
