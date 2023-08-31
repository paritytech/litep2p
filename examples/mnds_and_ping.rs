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

//! This examples demonstrates using mDNS to discover peers in the local network and
//! calculating their PING time.

use futures::{Stream, StreamExt};
use litep2p::{
    config::Litep2pConfigBuilder,
    protocol::{
        libp2p::ping::{PingConfig, PingEvent},
        mdns::{Config as MdnsConfig, MdnsEvent},
    },
    transport::tcp::config::TransportConfig as TcpTransportConfig,
    Litep2p,
};

use std::time::Duration;

// simple event loop which discovers peers over mDNS,
// establishes a connection to them and calculates the PING time
async fn peer_event_loop(
    mut litep2p: Litep2p,
    mut ping_event_stream: Box<dyn Stream<Item = PingEvent> + Send + Unpin>,
    mut mdns_event_stream: Box<dyn Stream<Item = MdnsEvent> + Send + Unpin>,
) {
    loop {
        tokio::select! {
            _ = litep2p.next_event() => {}
            event = ping_event_stream.next() => match event.unwrap() {
                PingEvent::Ping { peer, ping } => {
                    println!("ping received from {peer:?}: {ping:?}");
                }
            },
            event = mdns_event_stream.next() => match event.unwrap() {
                MdnsEvent::Discovered(addresses) => {
                    litep2p.dial_address(addresses[0].clone()).await.unwrap();
                }
            }
        }
    }
}

#[tokio::main]
async fn main() {
    // initialize IPFS ping and mDNS for the first peer
    let (ping_config1, ping_event_stream1) = PingConfig::default();
    let (mdns_config1, mdns_event_stream1) = MdnsConfig::new(Duration::from_secs(30));

    // initialize IPFS ping and mDNS for the second peer
    let (ping_config2, ping_event_stream2) = PingConfig::default();
    let (mdns_config2, mdns_event_stream2) = MdnsConfig::new(Duration::from_secs(30));

    // build `Litep2p` for the first peer
    let litep2p_config1 = Litep2pConfigBuilder::new()
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: yamux::Config::default(),
        })
        .with_libp2p_ping(ping_config1)
        .with_mdns(mdns_config1)
        .build();
    let litep2p1 = Litep2p::new(litep2p_config1).await.unwrap();

    // build `Litep2p` for the second peer
    let litep2p_config2 = Litep2pConfigBuilder::new()
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: yamux::Config::default(),
        })
        .with_libp2p_ping(ping_config2)
        .with_mdns(mdns_config2)
        .build();
    let litep2p2 = Litep2p::new(litep2p_config2).await.unwrap();

    // starts separate tasks for the first and second peer
    tokio::spawn(peer_event_loop(
        litep2p1,
        ping_event_stream1,
        mdns_event_stream1,
    ));
    tokio::spawn(peer_event_loop(
        litep2p2,
        ping_event_stream2,
        mdns_event_stream2,
    ));

    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
