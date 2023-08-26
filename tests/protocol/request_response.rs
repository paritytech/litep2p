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
    protocol::request_response::{
        RequestResponseConfig, RequestResponseError, RequestResponseEvent,
    },
    transport::tcp::config::TransportConfig as TcpTransportConfig,
    types::protocol::ProtocolName,
    Litep2p, Litep2pEvent,
};

use futures::StreamExt;
use tokio::time::sleep;

use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

async fn connect_peers(litep2p1: &mut Litep2p, litep2p2: &mut Litep2p) {
    let address = litep2p2.listen_addresses().next().unwrap().clone();
    litep2p1.connect(address).await.unwrap();

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

    sleep(Duration::from_millis(100)).await;
}

#[tokio::test]
async fn send_request_receive_response() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, mut handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // send request to remote peer
    let request_id = handle1.send_request(peer2, vec![1, 3, 3, 7]).await.unwrap();
    assert_eq!(
        handle2.next().await.unwrap(),
        RequestResponseEvent::RequestReceived {
            peer: peer1,
            request_id,
            request: vec![1, 3, 3, 7],
        }
    );

    // send response to the received request
    handle2
        .send_response(request_id, vec![1, 3, 3, 8])
        .await
        .unwrap();
    assert_eq!(
        handle1.next().await.unwrap(),
        RequestResponseEvent::ResponseReceived {
            peer: peer2,
            request_id,
            response: vec![1, 3, 3, 8],
        }
    );
}

#[tokio::test]
async fn reject_request() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, mut handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // send request to remote peer
    let request_id = handle1.send_request(peer2, vec![1, 3, 3, 7]).await.unwrap();
    if let RequestResponseEvent::RequestReceived {
        peer,
        request_id,
        request,
    } = handle2.next().await.unwrap()
    {
        assert_eq!(peer, peer1);
        assert_eq!(request, vec![1, 3, 3, 7]);
        handle2.reject_request(request_id).await;
    } else {
        panic!("invalid event received");
    };

    assert_eq!(
        handle1.next().await.unwrap(),
        RequestResponseEvent::RequestFailed {
            peer: peer2,
            request_id,
            error: RequestResponseError::Rejected
        }
    );
}

#[tokio::test]
async fn multiple_simultaneous_requests() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, mut handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // send multiple requests to remote peer
    let request_id1 = handle1.send_request(peer2, vec![1, 3, 3, 6]).await.unwrap();
    let request_id2 = handle1.send_request(peer2, vec![1, 3, 3, 7]).await.unwrap();
    let request_id3 = handle1.send_request(peer2, vec![1, 3, 3, 8]).await.unwrap();
    let request_id4 = handle1.send_request(peer2, vec![1, 3, 3, 9]).await.unwrap();
    let expected: HashMap<usize, Vec<u8>> = HashMap::from_iter([
        (request_id1, vec![2, 3, 3, 6]),
        (request_id2, vec![2, 3, 3, 7]),
        (request_id3, vec![2, 3, 3, 8]),
        (request_id4, vec![2, 3, 3, 9]),
    ]);

    for i in 0..4 {
        if let RequestResponseEvent::RequestReceived {
            peer,
            request_id,
            request,
        } = handle2.next().await.unwrap()
        {
            assert_eq!(peer, peer1);
            assert_eq!(request, vec![1, 3, 3, 6 + i]);
            handle2
                .send_response(request_id, vec![2, 3, 3, 6 + i])
                .await
                .unwrap();
        } else {
            panic!("invalid event received");
        };
    }

    for _ in 0..4 {
        if let RequestResponseEvent::ResponseReceived {
            peer,
            request_id,
            response,
        } = handle1.next().await.unwrap()
        {
            assert_eq!(peer, peer2);
            assert_eq!(response, expected.get(&request_id).unwrap().to_vec());
        } else {
            panic!("invalid event received");
        };
    }
}

#[tokio::test]
async fn request_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, _handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let _peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // send request to remote peer and wait until the requet timeout occurs
    let request_id = handle1.send_request(peer2, vec![1, 3, 3, 7]).await.unwrap();

    sleep(Duration::from_secs(7)).await;

    assert_eq!(
        handle1.next().await.unwrap(),
        RequestResponseEvent::RequestFailed {
            peer: peer2,
            request_id,
            error: RequestResponseError::Timeout,
        }
    );
}

#[tokio::test]
async fn protocol_not_supported() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, _handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/2"), 64, 1024);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let _peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // send request to remote peer and wait until the requet timeout occurs
    let request_id = handle1.send_request(peer2, vec![1, 3, 3, 7]).await.unwrap();

    assert_eq!(
        handle1.next().await.unwrap(),
        RequestResponseEvent::RequestFailed {
            peer: peer2,
            request_id,
            error: RequestResponseError::Rejected,
        }
    );
}

#[tokio::test]
async fn connection_close_while_request_is_pending() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let _peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            let _ = litep2p1.next_event().await;
        }
    });

    // send request to remote peer and wait until the requet timeout occurs
    let request_id = handle1.send_request(peer2, vec![1, 3, 3, 7]).await.unwrap();

    drop(handle2);
    drop(litep2p2);

    assert_eq!(
        handle1.next().await.unwrap(),
        RequestResponseEvent::RequestFailed {
            peer: peer2,
            request_id,
            error: RequestResponseError::Rejected,
        }
    );
}

#[tokio::test]
async fn request_too_big() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 256);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, _handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 256);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // try to send too large request to remote peer
    let request_id = handle1.send_request(peer2, vec![0u8; 257]).await.unwrap();
    assert_eq!(
        handle1.next().await.unwrap(),
        RequestResponseEvent::RequestFailed {
            peer: peer2,
            request_id,
            error: RequestResponseError::TooLargePayload,
        }
    );
}

#[tokio::test]
async fn response_too_big() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 256);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, mut handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 256);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: Default::default(),
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();

    let peer1 = *litep2p1.local_peer_id();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    // send request to remote peer
    let request_id = handle1.send_request(peer2, vec![0u8; 256]).await.unwrap();
    assert_eq!(
        handle2.next().await.unwrap(),
        RequestResponseEvent::RequestReceived {
            peer: peer1,
            request_id,
            request: vec![0u8; 256],
        }
    );

    // try to send too large response to the received request
    handle2
        .send_response(request_id, vec![0u8; 257])
        .await
        .unwrap();

    assert_eq!(
        handle1.next().await.unwrap(),
        RequestResponseEvent::RequestFailed {
            peer: peer2,
            request_id,
            error: RequestResponseError::Timeout,
        }
    );
}

#[tokio::test]
async fn too_many_pending_requests() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let mut yamux_config = yamux::Config::default();
    yamux_config.set_max_num_streams(4);

    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config,
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, _handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64, 1024);
    let mut yamux_config = yamux::Config::default();
    yamux_config.set_max_num_streams(4);

    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config,
        })
        .with_request_response_protocol(req_resp_config2)
        .build();

    let mut litep2p1 = Litep2p::new(config1).await.unwrap();
    let mut litep2p2 = Litep2p::new(config2).await.unwrap();
    let peer2 = *litep2p2.local_peer_id();

    // wait until peers have connected
    connect_peers(&mut litep2p1, &mut litep2p2).await;

    // send one over the max requests to remote peer
    let mut request_ids = HashSet::new();

    request_ids.insert(handle1.send_request(peer2, vec![1, 3, 3, 6]).await.unwrap());
    request_ids.insert(handle1.send_request(peer2, vec![1, 3, 3, 7]).await.unwrap());
    request_ids.insert(handle1.send_request(peer2, vec![1, 3, 3, 8]).await.unwrap());
    request_ids.insert(handle1.send_request(peer2, vec![1, 3, 3, 9]).await.unwrap());
    request_ids.insert(handle1.send_request(peer2, vec![1, 3, 3, 9]).await.unwrap());

    let mut litep2p1_closed = false;
    let mut litep2p2_closed = false;

    while !litep2p1_closed || !litep2p2_closed || !request_ids.is_empty() {
        tokio::select! {
            event = litep2p1.next_event() => match event {
                Some(Litep2pEvent::ConnectionClosed { .. }) => {
                    litep2p1_closed = true;
                }
                _ => {}
            },
            event = litep2p2.next_event() => match event {
                Some(Litep2pEvent::ConnectionClosed { .. }) => {
                    litep2p2_closed = true;
                }
                _ => {}
            },
            event = handle1.next() => match event {
                Some(RequestResponseEvent::RequestFailed {
                    request_id,
                    ..
                }) => {
                    request_ids.remove(&request_id);
                }
                _ => {}
            }
        }
    }
}
