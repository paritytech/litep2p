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
    config::Litep2pConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::notification::types::{
        Config as NotificationConfig, NotificationEvent, ValidationResult,
    },
    transport::tcp::config::TransportConfig as TcpTransportConfig,
    types::protocol::ProtocolName,
    Litep2p, Litep2pEvent,
};

async fn connect_peers(litep2p1: &mut Litep2p, litep2p2: &mut Litep2p) {
    let address = litep2p2.listen_addresses().next().unwrap().clone();
    litep2p1.connect(address).unwrap();

    let mut litep2p1_connected = false;
    let mut litep2p2_connected = false;

    loop {
        tokio::select! {
            event = litep2p1.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { .. } => {
                    litep2p1_connected = true;
                }
                _ => {},
            },
            event = litep2p2.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { .. } => {
                    litep2p2_connected = true;
                }
                _ => {},
            }
        }

        if litep2p1_connected && litep2p2_connected {
            break;
        }
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[tokio::test]
async fn open_substreams() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (notif_config1, mut handle1) = NotificationConfig::new(
        ProtocolName::from("/notif/1"),
        1024usize,
        vec![1, 2, 3, 4],
        Vec::new(),
    );
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        })
        .with_notification_protocol(notif_config1)
        .build();

    let (notif_config2, mut handle2) = NotificationConfig::new(
        ProtocolName::from("/notif/1"),
        1024usize,
        vec![1, 2, 3, 4],
        Vec::new(),
    );
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        })
        .with_notification_protocol(notif_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected and spawn the litep2p objects in the background
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // open substream for `peer2` and accept it
    handle1.open_substream(peer2).await;
    assert_eq!(
        handle2.next_event().await.unwrap(),
        NotificationEvent::ValidateSubstream {
            protocol: ProtocolName::from("/notif/1"),
            peer: peer1,
            handshake: vec![1, 2, 3, 4],
        }
    );
    handle2
        .send_validation_result(peer1, ValidationResult::Accept)
        .await;

    assert_eq!(
        handle2.next_event().await.unwrap(),
        NotificationEvent::NotificationStreamOpened {
            protocol: ProtocolName::from("/notif/1"),
            peer: peer1,
            handshake: vec![1, 2, 3, 4],
        }
    );
    assert_eq!(
        handle1.next_event().await.unwrap(),
        NotificationEvent::NotificationStreamOpened {
            protocol: ProtocolName::from("/notif/1"),
            peer: peer2,
            handshake: vec![1, 2, 3, 4],
        }
    );

    handle1.send_sync_notification(peer2, vec![1, 3, 3, 7]);
    handle2.send_sync_notification(peer1, vec![1, 3, 3, 8]);

    assert_eq!(
        handle2.next_event().await.unwrap(),
        NotificationEvent::NotificationReceived {
            peer: peer1,
            notification: vec![1, 3, 3, 7],
        }
    );
    assert_eq!(
        handle1.next_event().await.unwrap(),
        NotificationEvent::NotificationReceived {
            peer: peer2,
            notification: vec![1, 3, 3, 8],
        }
    );
}
