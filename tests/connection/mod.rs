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
    error::{AddressError, Error},
    peer_id::PeerId,
    protocol::libp2p::ping::PingConfig,
    transport::{
        quic::config::TransportConfig as QuicTransportConfig,
        tcp::config::TransportConfig as TcpTransportConfig,
    },
    Litep2p, Litep2pEvent,
};

use futures::StreamExt;
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use tokio::net::{TcpListener, UdpSocket};

#[tokio::test]
async fn two_litep2ps_work() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config1, _ping_event_stream1) = PingConfig::default();
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config1)
        .build();

    let (ping_config2, _ping_event_stream2) = PingConfig::default();
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

    let (res1, res2) = tokio::join!(litep2p1.next_event(), litep2p2.next_event());

    assert!(std::matches!(
        res1,
        Some(Litep2pEvent::ConnectionEstablished { .. })
    ));
    assert!(std::matches!(
        res2,
        Some(Litep2pEvent::ConnectionEstablished { .. })
    ));
}

#[tokio::test]
async fn dial_failure() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config1, _ping_event_stream1) = PingConfig::default();
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config1)
        .build();

    let (ping_config2, _ping_event_stream2) = PingConfig::default();
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

    let address = Multiaddr::empty()
        .with(Protocol::Ip6(std::net::Ipv6Addr::new(
            0, 0, 0, 0, 0, 0, 0, 1,
        )))
        .with(Protocol::Tcp(1))
        .with(Protocol::P2p(
            Multihash::from_bytes(&litep2p2.local_peer_id().to_bytes()).unwrap(),
        ));

    litep2p1.connect(address).await.unwrap();

    tokio::spawn(async move {
        loop {
            let _ = litep2p2.next_event().await;
        }
    });

    assert!(std::matches!(
        litep2p1.next_event().await,
        Some(Litep2pEvent::DialFailure { .. })
    ));
}

#[tokio::test]
async fn connect_over_dns() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let keypair1 = Keypair::generate();
    let (ping_config1, _ping_event_stream1) = PingConfig::default();

    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(keypair1)
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config1)
        .build();

    let keypair2 = Keypair::generate();
    let (ping_config2, _ping_event_stream2) = PingConfig::default();

    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(keypair2)
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip4/127.0.0.1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();
    let peer2 = *litep2p2.local_peer_id();

    let address = litep2p2.listen_addresses().next().unwrap().clone();
    let tcp = address.iter().skip(1).next().unwrap();

    let mut new_address = Multiaddr::empty();
    new_address.push(Protocol::Dns("localhost".into()));
    new_address.push(tcp);
    new_address.push(Protocol::P2p(
        Multihash::from_bytes(&peer2.to_bytes()).unwrap(),
    ));

    litep2p1.connect(new_address).await.unwrap();
    let (res1, res2) = tokio::join!(litep2p1.next_event(), litep2p2.next_event());

    assert!(std::matches!(
        res1,
        Some(Litep2pEvent::ConnectionEstablished { .. })
    ));
    assert!(std::matches!(
        res2,
        Some(Litep2pEvent::ConnectionEstablished { .. })
    ));
}

#[tokio::test]
async fn connection_timeout_tcp() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::default();
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config)
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

    let Some(Litep2pEvent::DialFailure {
        address: dial_address,
        error,
    }) = litep2p.next_event().await
    else {
        panic!("invalid event received");
    };

    assert_eq!(dial_address, address);
    assert!(std::matches!(error, Error::Timeout));
}

#[tokio::test]
async fn connection_timeout_quic() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::default();
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_quic(QuicTransportConfig {
            listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        })
        .with_libp2p_ping(ping_config)
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

    let Some(Litep2pEvent::DialFailure {
        address: dial_address,
        error,
    }) = litep2p.next_event().await
    else {
        panic!("invalid event received");
    };

    assert_eq!(dial_address, address);
    assert!(std::matches!(error, Error::TransportError(_)));
}

#[tokio::test]
async fn dial_quic_peer_id_missing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::default();
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_quic(QuicTransportConfig {
            listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        })
        .with_libp2p_ping(ping_config)
        .build();

    let mut litep2p = Litep2p::new(config).await.unwrap();

    // create udp socket but don't respond to any inbound datagrams
    let listener = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let address = Multiaddr::empty()
        .with(Protocol::from(address.ip()))
        .with(Protocol::Udp(address.port()))
        .with(Protocol::QuicV1);

    match litep2p.connect(address.clone()).await {
        Err(Error::AddressError(AddressError::PeerIdMissing)) => {}
        _ => panic!("dial not supposed to succeed"),
    }
}

#[tokio::test]
async fn dial_self_tcp() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::default();
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config)
        .build();

    let mut litep2p = Litep2p::new(config).await.unwrap();

    let address = litep2p.listen_addresses().next().unwrap().clone();

    // dial without peer id attached
    assert!(std::matches!(
        litep2p.connect(address.clone()).await,
        Err(Error::TriedToDialSelf)
    ));
}

#[tokio::test]
async fn dial_self_quic() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::default();
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_quic(QuicTransportConfig {
            listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        })
        .with_libp2p_ping(ping_config)
        .build();

    let mut litep2p = Litep2p::new(config).await.unwrap();

    let address = litep2p.listen_addresses().next().unwrap().clone();

    // dial without peer id attached
    assert!(std::matches!(
        litep2p.connect(address.clone()).await,
        Err(Error::TriedToDialSelf)
    ));
}

#[tokio::test]
async fn attempt_to_dial_using_unsupported_transport() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config, _ping_event_stream) = PingConfig::default();
    let config = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_quic(QuicTransportConfig {
            listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        })
        .with_libp2p_ping(ping_config)
        .build();

    let mut litep2p = Litep2p::new(config).await.unwrap();
    let address = Multiaddr::empty()
        .with(Protocol::from(std::net::Ipv4Addr::new(127, 0, 0, 1)))
        .with(Protocol::Tcp(8888))
        .with(Protocol::P2p(
            Multihash::from_bytes(&PeerId::random().to_bytes()).unwrap(),
        ));

    assert!(std::matches!(
        litep2p.connect(address.clone()).await,
        Err(Error::TransportNotSupported(_))
    ));
}

#[tokio::test]
async fn keep_alive_timeout_tcp() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (ping_config1, mut ping_event_stream1) = PingConfig::default();
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config1)
        .build();
    let mut litep2p1 = Litep2p::new(config1).await.unwrap();

    let (ping_config2, mut ping_event_stream2) = PingConfig::default();
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_libp2p_ping(ping_config2)
        .build();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let address1 = litep2p1.listen_addresses().next().unwrap().clone();
    litep2p2.connect(address1).await.unwrap();
    let mut litep2p1_ping = false;
    let mut litep2p2_ping = false;

    loop {
        tokio::select! {
            event = litep2p1.next_event() => match event {
                Some(Litep2pEvent::ConnectionClosed { .. }) if litep2p1_ping || litep2p2_ping => {
                    break;
                }
                _ => {}
            },
            event = litep2p2.next_event() => match event {
                Some(Litep2pEvent::ConnectionClosed { .. }) if litep2p1_ping || litep2p2_ping => {
                    break;
                }
                _ => {}
            },
            _event = ping_event_stream1.next() => {
                litep2p1_ping = true;
            }
            _event = ping_event_stream2.next() => {
                litep2p2_ping = true;
            }
        }
    }
}
