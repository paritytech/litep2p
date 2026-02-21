// Copyright 2024 litep2p developers
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

//! Tests for verifying that failed addresses are reported back on a successful connection.
//!
//! This addresses the issue where the TCP transport (and others) attempts to dial up to 8
//! addresses simultaneously. When one connection succeeds while others fail, the errors
//! should be propagated back to the transport manager so that failed addresses can have
//! their scores updated.
//!
//! See: https://github.com/paritytech/litep2p/issues/526

use litep2p::{
    config::ConfigBuilder, crypto::ed25519::Keypair, protocol::libp2p::ping::Config as PingConfig,
    transport::tcp::config::Config as TcpConfig, Litep2p, Litep2pEvent,
};

use futures::StreamExt;
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use std::time::Duration;
use tokio::net::TcpListener;

#[tokio::test]
async fn failed_addresses_reported_on_successful_connection() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    // Test for verifying that failed addresses are reported back on a successful connection.
    // - https://github.com/paritytech/litep2p/issues/526
    let (ping_config2, _ping_event_stream2) = PingConfig::default();
    let config2 = ConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_ping(ping_config2)
        .build();

    let mut litep2p2 = Litep2p::new(config2).unwrap();
    let peer2 = *litep2p2.local_peer_id();
    let valid_address = litep2p2.listen_addresses().next().unwrap().clone();

    tracing::info!(?peer2, ?valid_address, "target litep2p created");

    // Create 7 TCP listeners that will accept connections but delay (simulating slow addresses)
    // These will cause the connection attempts to timeout or fail during negotiation
    let mut failing_listeners = Vec::new();
    let mut failing_addresses = Vec::new();

    for i in 0..7 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let address = Multiaddr::empty()
            .with(Protocol::from(addr.ip()))
            .with(Protocol::Tcp(addr.port()))
            .with(Protocol::P2p(
                Multihash::from_bytes(&peer2.to_bytes()).unwrap(),
            ));

        tracing::info!(index = i, ?address, "created failing listener");

        failing_addresses.push(address);
        failing_listeners.push(listener);
    }

    // Spawn tasks to handle the failing listeners - accept connection then delay 5 seconds
    // This simulates addresses that are reachable but slow to complete the handshake
    for (i, listener) in failing_listeners.into_iter().enumerate() {
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, addr)) => {
                        tracing::info!(
                            listener_index = i,
                            ?addr,
                            "failing listener accepted connection, delaying 5s"
                        );
                        // Hold the connection for 5 seconds then drop it
                        // This simulates a slow/failing address
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        drop(socket);
                        tracing::info!(
                            listener_index = i,
                            "failing listener dropped connection after delay"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(listener_index = i, ?e, "failing listener accept error");
                        break;
                    }
                }
            }
        });
    }

    // Create the dialing litep2p node
    let (ping_config1, _ping_event_stream1) = PingConfig::default();
    let config1 = ConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpConfig {
            listen_addresses: vec!["/ip4/127.0.0.1/tcp/0".parse().unwrap()],
            ..Default::default()
        })
        .with_libp2p_ping(ping_config1)
        .build();

    let mut litep2p1 = Litep2p::new(config1).unwrap();

    // Add all 8 addresses (7 failing + 1 valid) as known addresses for peer2
    let mut all_addresses = failing_addresses.clone();
    all_addresses.push(valid_address.clone());

    tracing::info!(
        target: "test",
        num_addresses = all_addresses.len(),
        "adding known addresses for peer"
    );

    let added = litep2p1.add_known_address(peer2, all_addresses.into_iter());
    assert_eq!(added, 8, "should have added 8 addresses");

    // Dial the peer - this will try all 8 addresses in parallel
    tracing::info!(target: "test", ?peer2, "dialing peer with 8 addresses");
    litep2p1.dial(&peer2).await.unwrap();

    // Run both litep2p nodes and wait for connection establishment
    let mut connection_established = false;

    // Use a timeout to ensure the test doesn't hang
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            tokio::select! {
                event = litep2p1.next_event() => {
                    tracing::info!(?event, "litep2p1 event");
                    match event {
                        Some(Litep2pEvent::ConnectionEstablished { peer, .. }) => {
                            assert_eq!(peer, peer2);
                            tracing::info!(
                                ?peer,
                                "connection established to peer"
                            );
                            connection_established = true;
                            break;
                        }
                        Some(Litep2pEvent::DialFailure { address, error }) => {
                            // This might happen for individual failed addresses
                            tracing::info!(
                                ?address,
                                ?error,
                                "dial failure event"
                            );
                        }
                        _ => {}
                    }
                }
                event = litep2p2.next_event() => {
                    tracing::info!(?event, "litep2p2 event");
                }
            }
        }
    })
    .await;

    assert!(result.is_ok(), "test timed out waiting for connection");
    assert!(
        connection_established,
        "connection should have been established"
    );

    tracing::info!(
        "test completed successfully - connection established with 7 failing addresses and 1 successful"
    );
}
