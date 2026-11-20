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

use super::*;
use crate::{config::ConfigBuilder, transport::tcp::config::Config as TcpConfig, Litep2p};
use multiaddr::{Multiaddr, Protocol};

fn create_litep2p() -> (
    Litep2p,
    Box<dyn Stream<Item = IdentifyEvent> + Send + Unpin>,
    PeerId,
) {
    let (identify_config, identify) =
        Config::new("1.0.0".to_string(), Some("litep2p/1.0.0".to_string()));

    let keypair = crate::crypto::ed25519::Keypair::generate();
    let peer = PeerId::from_public_key(&crate::crypto::PublicKey::Ed25519(keypair.public()));
    let config = ConfigBuilder::new()
        .with_keypair(keypair)
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_identify(identify_config)
        .build();

    (Litep2p::new(config).unwrap(), identify, peer)
}

#[tokio::test]
async fn update_identify_addresses() {
    // Create two instances of litep2p
    let (mut litep2p1, mut event_stream1, peer1) = create_litep2p();
    let (mut litep2p2, mut event_stream2, _peer2) = create_litep2p();
    let litep2p1_address = litep2p1.listen_addresses().next().unwrap();

    let multiaddr: Multiaddr = "/ip6/::9/tcp/111".parse().unwrap();
    // Litep2p1 is now reporting the new address.
    assert!(litep2p1.public_addresses().add_address(multiaddr.clone()).unwrap());

    // Dial `litep2p1`
    litep2p2.dial_address(litep2p1_address.clone()).await.unwrap();

    let expected_multiaddr = multiaddr.with(Protocol::P2p(peer1.into()));

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {}
                _event = event_stream1.next() => {}
            }
        }
    });

    loop {
        tokio::select! {
            _ = litep2p2.next_event() => {}
            event = event_stream2.next() => match event {
                Some(IdentifyEvent::PeerIdentified {
                    listen_addresses,
                    ..
                }) => {
                    assert!(listen_addresses.iter().any(|address| address == &expected_multiaddr));
                    break;
                }
                _ => {}
            }
        }
    }
}

#[tokio::test]
async fn listen_addresses_direct_dials_at_third_party() {
    let accepted = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut victim_ports = Vec::new();
    for _ in 0..40 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        victim_ports.push(listener.local_addr().unwrap().port());

        let accepted = accepted.clone();
        tokio::spawn(async move {
            while listener.accept().await.is_ok() {
                accepted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
    }

    let (mut litep2p1, mut event_stream1, _peer1) = create_litep2p();
    let (mut litep2p2, _event_stream2, peer2) = create_litep2p();
    for port in victim_ports {
        let address: Multiaddr = format!("/ip4/127.0.0.1/tcp/{port}").parse().unwrap();
        litep2p2.public_addresses().add_address(address).unwrap();
    }

    let address = litep2p2.listen_addresses().next().unwrap().clone();
    litep2p1.dial_address(address).await.unwrap();

    let attacker = tokio::spawn(async move { while litep2p2.next_event().await.is_some() {} });

    let listen_addresses = loop {
        tokio::select! {
            _ = litep2p1.next_event() => {}
            event = event_stream1.next() => {
                let Some(IdentifyEvent::PeerIdentified { listen_addresses, .. }) = event else {
                    panic!("identify stream ended");
                };
                break listen_addresses;
            }
        }
    };
    assert!(listen_addresses.len() > 40);
    litep2p1.add_known_address(peer2, listen_addresses.into_iter());

    attacker.abort();
    while !std::matches!(
        litep2p1.next_event().await,
        Some(crate::Litep2pEvent::ConnectionClosed { .. })
    ) {}

    litep2p1.dial(&peer2).await.unwrap();
    while !std::matches!(
        litep2p1.next_event().await,
        Some(
            crate::Litep2pEvent::DialFailure { .. } | crate::Litep2pEvent::ListDialFailures { .. }
        )
    ) {}

    assert_eq!(accepted.load(std::sync::atomic::Ordering::SeqCst), 40);
}

fn raw(addresses: &[&str]) -> Vec<Vec<u8>> {
    addresses
        .iter()
        .map(|address| address.parse::<Multiaddr>().unwrap().to_vec())
        .collect()
}

#[test]
fn undialable_listen_addresses_are_dropped() {
    let dialable = [
        "/ip4/1.1.1.1/tcp/30333",
        "/dns/example.com/tcp/443/wss",
        "/ip6/2001:db8::1/udp/30333/quic-v1",
    ];
    let mut addresses = raw(&[
        "/ip4/0.0.0.0/tcp/30333",
        "/ip6/::/tcp/30333",
        "/ip4/224.0.0.1/tcp/30333",
        "/ip4/255.255.255.255/tcp/30333",
        "/ip6/ff02::1/tcp/30333",
        "/ip4/1.1.1.1/tcp/0",
        "/ip4/1.1.1.1",
        "/tcp/30333",
        "/dnsaddr/example.com",
    ]);
    addresses.extend(raw(&dialable));
    addresses.push(vec![0xff, 0xff]);

    let peer = PeerId::random();
    let expected: Vec<Multiaddr> = dialable
        .iter()
        .map(|address| address.parse::<Multiaddr>().unwrap().with(Protocol::P2p(peer.into())))
        .collect();
    assert_eq!(parse_listen_addresses(peer, addresses), expected);
}

#[test]
fn listen_addresses_are_deduplicated() {
    let peer = PeerId::random();
    let address: Multiaddr = "/ip4/1.1.1.1/tcp/30333".parse().unwrap();
    let with_peer = address.clone().with(Protocol::P2p(peer.into()));

    let addresses = vec![address.to_vec(), with_peer.to_vec(), address.to_vec()];
    assert_eq!(parse_listen_addresses(peer, addresses), vec![with_peer]);
}
