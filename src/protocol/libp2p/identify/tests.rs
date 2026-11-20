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
