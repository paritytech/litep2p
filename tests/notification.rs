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
    config::LiteP2pConfiguration,
    protocol::notification::{
        NotificationEvent, NotificationProtocolConfig, NotificationService, ValidationResult,
    },
    Litep2p, Litep2pEvent,
};
use multiaddr::Multiaddr;

async fn initialize_litep2p(
    addr1: Multiaddr,
    addr2: Multiaddr,
) -> (
    (Litep2p, NotificationService),
    (Litep2p, NotificationService),
) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (config1, service1) =
        NotificationProtocolConfig::new("/notification/1".to_owned(), vec![1, 3, 3, 7]);
    let (config2, service2) =
        NotificationProtocolConfig::new("/notification/1".to_owned(), vec![1, 3, 3, 7]);

    let mut litep2p1 = Litep2p::new(LiteP2pConfiguration::new(
        vec![addr1.clone()],
        vec![config1],
        vec![],
    ))
    .await
    .unwrap();

    let mut litep2p2 = Litep2p::new(LiteP2pConfiguration::new(
        vec![addr2.clone()],
        vec![config2],
        vec![],
    ))
    .await
    .unwrap();

    // attempt to open connection to `litep2p` and verify that both got the event
    litep2p1.open_connection(addr2).await.unwrap();

    let (peer1, peer2) = match tokio::join!(litep2p1.next_event(), litep2p2.next_event()) {
        (
            Ok(Litep2pEvent::ConnectionEstablished(peer2)),
            Ok(Litep2pEvent::ConnectionEstablished(peer1)),
        ) => {
            assert_eq!(peer2, *litep2p2.local_peer_id());
            assert_eq!(peer1, *litep2p1.local_peer_id());
            (peer1, peer2)
        }
        event => panic!("invalid event {event:?}"),
    };

    // verify that both peers are identified by each other
    match tokio::join!(litep2p1.next_event(), litep2p2.next_event()) {
        (
            Ok(Litep2pEvent::PeerIdentified {
                peer: identified_peer2,
                supported_protocols: supported_protocols1,
            }),
            Ok(Litep2pEvent::PeerIdentified {
                peer: identified_peer1,
                supported_protocols: supported_protocols2,
            }),
        ) => {
            assert_eq!(peer1, identified_peer1);
            assert_eq!(peer2, identified_peer2);
            assert_eq!(supported_protocols1, supported_protocols2);
        }
        event => panic!("invalid event {event:?}"),
    }

    ((litep2p1, service1), (litep2p2, service2))
}

#[tokio::test]
async fn notification_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (config1, mut service1) =
        NotificationProtocolConfig::new("/notification/1".to_owned(), vec![1, 3, 3, 7]);
    let (config2, mut service2) =
        NotificationProtocolConfig::new("/notification/1".to_owned(), vec![1, 3, 3, 7]);

    let addr1: Multiaddr = "/ip6/::1/tcp/8888".parse().expect("valid multiaddress");
    let mut litep2p1 = Litep2p::new(LiteP2pConfiguration::new(
        vec![addr1.clone()],
        vec![config1],
        vec![],
    ))
    .await
    .unwrap();

    let addr2: Multiaddr = "/ip6/::1/tcp/8889".parse().expect("valid multiaddress");
    let mut litep2p2 = Litep2p::new(LiteP2pConfiguration::new(
        vec![addr2.clone()],
        vec![config2],
        vec![],
    ))
    .await
    .unwrap();

    // attempt to open connection to `litep2p` and verify that both got the event
    litep2p1.open_connection(addr2).await.unwrap();

    let (peer1, peer2) = match tokio::join!(litep2p1.next_event(), litep2p2.next_event()) {
        (
            Ok(Litep2pEvent::ConnectionEstablished(peer2)),
            Ok(Litep2pEvent::ConnectionEstablished(peer1)),
        ) => {
            assert_eq!(peer2, *litep2p2.local_peer_id());
            assert_eq!(peer1, *litep2p1.local_peer_id());
            (peer1, peer2)
        }
        event => panic!("invalid event {event:?}"),
    };

    // verify that both peers are identified by each other
    match tokio::join!(litep2p1.next_event(), litep2p2.next_event()) {
        (
            Ok(Litep2pEvent::PeerIdentified {
                peer: identified_peer2,
                supported_protocols: supported_protocols1,
            }),
            Ok(Litep2pEvent::PeerIdentified {
                peer: identified_peer1,
                supported_protocols: supported_protocols2,
            }),
        ) => {
            assert_eq!(peer1, identified_peer1);
            assert_eq!(peer2, identified_peer2);
            assert_eq!(supported_protocols1, supported_protocols2);
        }
        event => panic!("invalid event {event:?}"),
    }

    // poll both `Litep2p` instances in the background
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    let peer1_received = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let peer1_signal = std::sync::Arc::clone(&peer1_received);

    tokio::spawn(async move {
        // attempt to open substream to remote peer.
        service1.open_substream(peer2).await.unwrap();

        while let Some(event) = service1.next_event().await {
            match event {
                NotificationEvent::SubstreamReceived { peer, handshake: _ } => {
                    service1
                        .report_validation_result(peer, ValidationResult::Accept)
                        .await
                        .unwrap();
                }
                NotificationEvent::SubstreamOpened { .. } => {
                    service1
                        .send_sync_notification(peer2, vec![11, 22, 33, 44])
                        .unwrap();
                }
                NotificationEvent::NotificationReceived { peer, notification } => {
                    assert_eq!(peer, peer2);
                    assert_eq!(notification, vec![55, 66, 77, 88]);
                    peer1_signal.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                event => panic!("service1: unhandled event {event:?}"),
            }
        }
    });

    while let Some(event) = service2.next_event().await {
        match event {
            NotificationEvent::SubstreamReceived { peer, handshake: _ } => {
                service2
                    .report_validation_result(peer, ValidationResult::Accept)
                    .await
                    .unwrap();
            }
            NotificationEvent::SubstreamOpened { .. } => {}
            NotificationEvent::NotificationReceived { peer, notification } => {
                assert_eq!(peer, peer1);
                assert_eq!(notification, vec![11, 22, 33, 44]);
                service2
                    .send_async_notification(peer1, vec![55, 66, 77, 88])
                    .await
                    .unwrap();
                break;
            }
            event => panic!("service2: unhandled event {event:?}"),
        }
    }

    while !peer1_received.load(std::sync::atomic::Ordering::SeqCst) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

#[tokio::test]
async fn reject_inbound_substream() {
    let addr1: Multiaddr = "/ip6/::1/tcp/2222".parse().expect("valid multiaddress");
    let addr2: Multiaddr = "/ip6/::1/tcp/2223".parse().expect("valid multiaddress");

    let ((mut litep2p1, mut service1), (mut litep2p2, mut service2)) =
        initialize_litep2p(addr1, addr2).await;
    let peer2 = *litep2p2.local_peer_id();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    tokio::spawn(async move {
        // attempt to open substream to remote peer.
        while let Some(event) = service2.next_event().await {
            match event {
                NotificationEvent::SubstreamReceived { peer, handshake: _ } => {
                    service2
                        .report_validation_result(peer, ValidationResult::Reject)
                        .await
                        .unwrap();
                }
                event => panic!("service2: unhandled event {event:?}"),
            }
        }
    });

    service1.open_substream(peer2).await.unwrap();

    while let Some(event) = service1.next_event().await {
        match event {
            NotificationEvent::SubstreamRejected { peer } => {
                assert_eq!(peer, peer2);
                break;
            }
            event => panic!("service1: unhandled event {event:?}"),
        }
    }
}

#[tokio::test]
async fn accept_inbound_substream() {
    let addr1: Multiaddr = "/ip6/::1/tcp/2224".parse().expect("valid multiaddress");
    let addr2: Multiaddr = "/ip6/::1/tcp/2225".parse().expect("valid multiaddress");

    let ((mut litep2p1, mut service1), (mut litep2p2, mut service2)) =
        initialize_litep2p(addr1, addr2).await;
    let peer2 = *litep2p2.local_peer_id();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    tokio::spawn(async move {
        // attempt to open substream to remote peer.
        while let Some(event) = service2.next_event().await {
            match event {
                NotificationEvent::SubstreamReceived { peer, handshake: _ } => {
                    service2
                        .report_validation_result(peer, ValidationResult::Accept)
                        .await
                        .unwrap();
                }
                event => panic!("service2: unhandled event {event:?}"),
            }
        }
    });

    service1.open_substream(peer2).await.unwrap();

    while let Some(event) = service1.next_event().await {
        match event {
            NotificationEvent::SubstreamOpened { peer, .. } => {
                assert_eq!(peer, peer2);
                break;
            }
            event => panic!("service1: unhandled event {event:?}"),
        }
    }
}

#[tokio::test]
async fn both_nodes_open_substream() {
    let addr1: Multiaddr = "/ip6/::1/tcp/2224".parse().expect("valid multiaddress");
    let addr2: Multiaddr = "/ip6/::1/tcp/2225".parse().expect("valid multiaddress");

    let ((mut litep2p1, mut service1), (mut litep2p2, mut service2)) =
        initialize_litep2p(addr1, addr2).await;
    let peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    let (res1, res2) = tokio::join!(
        service1.open_substream(peer2),
        service2.open_substream(peer1),
    );
    let _ = res1.unwrap();
    let _ = res2.unwrap();
    // service2.open_substream(peer1).await.unwrap();

    loop {
        tokio::select! {
            event = service1.next_event() => match event.unwrap() {
                event => tracing::warn!(target: "notification", "service1: {event:?}")
            },
            event = service2.next_event() => match event.unwrap() {
                event => tracing::warn!(target: "notification", "service2: {event:?}")
            }
        }
    }
}

#[tokio::test]
async fn notification_streams_exhausted() {}

#[tokio::test]
async fn node_tries_to_open_substream_twice() {}

#[tokio::test]
async fn node_disconnects_while_negotiation_is_in_progress() {}

#[tokio::test]
async fn sync_notifications_clogged() {}

#[tokio::test]
async fn async_notifications_clogged() {}

#[tokio::test]
async fn set_new_handshake() {}

#[tokio::test]
async fn send_sync_notification_to_non_existent_peer() {}

#[tokio::test]
async fn send_async_notification_to_non_existent_peer() {}

#[tokio::test]
async fn send_sync_notification_on_closed_substream() {}

#[tokio::test]
async fn send_async_notification_on_closed_substream() {
    let addr1: Multiaddr = "/ip6/::1/tcp/2226".parse().expect("valid multiaddress");
    let addr2: Multiaddr = "/ip6/::1/tcp/2227".parse().expect("valid multiaddress");

    let ((mut litep2p1, mut service1), (mut litep2p2, mut service2)) =
        initialize_litep2p(addr1, addr2).await;
    let peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    service1.open_substream(peer2).await.unwrap();

    loop {
        tokio::select! {
            event = service2.next_event() => match event.unwrap() {
                NotificationEvent::SubstreamReceived { peer, handshake: _ } => {
                    service2
                        .report_validation_result(peer, ValidationResult::Accept)
                        .await
                        .unwrap();
                }
                _ => {},
            },
            event = service1.next_event() => match event.unwrap() {
                NotificationEvent::SubstreamOpened { peer, .. } => {
                    assert_eq!(peer, peer2);
                    break;
                }
                _ => {},
            }
        }
    }

    service2.close_substream(peer1).unwrap();

    for i in 0..5 {
        let res = service1
            .send_async_notification(peer2, vec![1, 3, 3, 7])
            .await;
        tracing::info!("res: {res:?}");
    }
}
