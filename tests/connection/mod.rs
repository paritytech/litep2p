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
    error::Error,
    peer_id::PeerId,
    protocol::libp2p::ping::Config as PingConfig,
    transport::{
        quic::config::Config as QuicTransportConfig,
        tcp::config::TransportConfig as TcpTransportConfig,
    },
    Litep2p, Litep2pEvent,
};
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use tokio::net::{TcpListener, UdpSocket};

#[tokio::test]
async fn connection_timeout_tcp() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::new(3);
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        })
        .with_ipfs_ping(ping_config)
        .build();

    let mut litep2p = Litep2p::new(config).await.unwrap();

    // create tcp listener but don't accept any inbound connections
    let listener = TcpListener::bind("[::1]:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let address = Multiaddr::empty()
        .with(Protocol::from(address.ip()))
        .with(Protocol::Tcp(address.port()))
        .with(Protocol::P2p(
            Multihash::from_bytes(&PeerId::random().to_bytes()).unwrap(),
        ));

    litep2p.connect(address.clone()).await.unwrap();

    let Ok(Litep2pEvent::DialFailure { address: dial_address, error }) = litep2p.next_event().await else {
        panic!("invalid event received");
    };

    assert_eq!(dial_address, address);
    assert!(std::matches!(error, Error::Timeout));
}

#[tokio::test]
async fn connection_timeout_quic_handshake() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::new(3);
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_quic(QuicTransportConfig {
            listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        })
        .with_ipfs_ping(ping_config)
        .build();

    let mut litep2p = Litep2p::new(config).await.unwrap();

    // create udp socket but don't respond to any inbound datagrams
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let address = Multiaddr::empty()
        .with(Protocol::from(address.ip()))
        .with(Protocol::Udp(address.port()))
        .with(Protocol::QuicV1)
        .with(Protocol::P2p(
            Multihash::from_bytes(&PeerId::random().to_bytes()).unwrap(),
        ));

    litep2p.connect(address.clone()).await.unwrap();

    let Ok(Litep2pEvent::DialFailure { address: dial_address, error }) = litep2p.next_event().await else {
        panic!("invalid event received");
    };

    assert_eq!(dial_address, address);
    assert!(std::matches!(error, Error::TransportError(_)));
}
