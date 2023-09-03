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
    config::Litep2pConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::libp2p::{identify::Config as IdentifyConfig, ping::Config as PingConfig},
    transport::tcp::config::TransportConfig as TcpTransportConfig,
    Litep2p,
};

use std::{io::Write, os::unix::net::UnixStream, path::PathBuf};

#[tokio::test]
#[cfg(debug_assertions)]
async fn go_libp2p_dials() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let mut file_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    file_path.push("tests/conformance/golang/tcp_ping_identify/ping_test");

    let _ = std::process::Command::new(file_path)
        .spawn()
        .expect("to succeed");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let mut stream = UnixStream::connect("/tmp/ping-test.sock").unwrap();

    let keypair = Keypair::generate();
    let (ping_config, mut ping_event_stream) = PingConfig::default();
    let (identify_config, mut identify_event_stream) = IdentifyConfig::new();

    let mut litep2p = Litep2p::new(
        Litep2pConfigBuilder::new()
            .with_keypair(keypair)
            .with_tcp(TcpTransportConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
                yamux_config: Default::default(),
            })
            .with_libp2p_ping(ping_config)
            .with_libp2p_identify(identify_config)
            .build(),
    )
    .await
    .unwrap();

    let peer = *litep2p.local_peer_id();
    let mut address = litep2p.listen_addresses().next().unwrap().clone();
    address.push(multiaddr::Protocol::P2p((peer).into()));

    tracing::info!("address: {address:?}");

    let address = address.to_string();
    let address = address.as_bytes();

    tracing::info!("length {}", address.len());

    let length_bytes: [u8; 2] = [(address.len() >> 8) as u8, address.len() as u8];

    stream.write_all(&length_bytes).unwrap();
    stream.write_all(address).unwrap();

    // stream.read(&mut len).unwrap();
    // let addr_len = u16::from_be_bytes([len[0], len[1]]);
    // tracing::info!("string length {addr_len}");

    // let mut address = vec![0u8; addr_len as usize];
    // stream.read_exact(&mut address).unwrap();

    // tracing::info!("{:?}", std::str::from_utf8(&address).unwrap());
    // tracing::info!("{:?}", address);

    // litep2p.connect(address).unwrap();

    tokio::spawn(async move {
        loop {
            let _ = litep2p.next_event().await;
        }
    });

    let mut ping_received = false;
    let mut identify_received = false;

    loop {
        tokio::select! {
            _event = ping_event_stream.next() => {
                ping_received = true;
            }
            _event = identify_event_stream.next() => {
                identify_received = true;
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("failed to receive ping in time");
            }
        }

        if identify_received && ping_received {
            break;
        }
    }

    // drop the UDS connection and give the Golang program a little time to clean up resources
    drop(stream);
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
}
