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
use litep2p::{
    config::Litep2pConfigBuilder, crypto::ed25519::Keypair,
    protocol::libp2p::ping::Config as PingConfig,
    transport::tcp::config::TransportConfig as TcpTransportConfig, Litep2p,
};

#[tokio::test]
async fn ping_supported() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config1, mut ping_event_stream1) = PingConfig::new(3);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config1)
        .build();

    let (ping_config2, mut ping_event_stream2) = PingConfig::new(3);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();
    let address = litep2p2.listen_addresses().next().unwrap().clone();

    litep2p1.connect(address).await.unwrap();

    let mut litep2p1_done = false;
    let mut litep2p2_done = false;

    loop {
        tokio::select! {
            _event = litep2p1.next_event() => {}
            _event = litep2p2.next_event() => {}
            _event = ping_event_stream1.next() => {
                litep2p1_done = true;

                if litep2p1_done && litep2p2_done {
                    break
                }
            }
            _event = ping_event_stream2.next() => {
                litep2p2_done = true;

                if litep2p1_done && litep2p2_done {
                    break
                }
            }
        }
    }
}
