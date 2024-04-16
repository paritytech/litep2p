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

use futures::{channel::oneshot, stream::FuturesUnordered, StreamExt};
use libp2p::{
    identity,
    swarm::{SwarmBuilder, SwarmEvent},
    PeerId, Swarm,
};
use litep2p::{
    config::ConfigBuilder,
    protocol::request_response::{
        RequestResponseConfig, RequestResponseError, RequestResponseEvent, RequestResponseHandle,
    },
    transport::tcp::config::Config as TcpConfig,
    Litep2p, Litep2pEvent,
};
use sc_network::{
    peer_store::PeerStore,
    request_responses::{
        IncomingRequest, OutgoingResponse, ProtocolConfig, RequestFailure,
        RequestResponsesBehaviour,
    },
    types::ProtocolName,
    IfDisconnected, OutboundFailure,
};

fn initialize_libp2p() -> (
    Swarm<RequestResponsesBehaviour>,
    PeerStore,
    async_channel::Receiver<IncomingRequest>,
) {
    let local_key = identity::Keypair::generate_ed25519();
    let local_peer_id = PeerId::from(local_key.public());
    let peer_store = PeerStore::new(vec![]);
    let peer_store_handle = Box::new(peer_store.handle());

    let (tx, rx) = async_channel::bounded(64);
    let configs = vec![ProtocolConfig {
        name: ProtocolName::from("/request/1"),
        fallback_names: Vec::new(),
        max_request_size: 256,
        max_response_size: 2 * 256,
        request_timeout: std::time::Duration::from_secs(10),
        inbound_queue: Some(tx),
    }];

    let behaviour = RequestResponsesBehaviour::new(configs.into_iter(), peer_store_handle).unwrap();

    let transport = libp2p::tokio_development_transport(local_key).unwrap();
    let mut swarm = SwarmBuilder::with_tokio_executor(transport, behaviour, local_peer_id).build();

    swarm.listen_on("/ip6/::1/tcp/0".parse().unwrap()).unwrap();

    (swarm, peer_store, rx)
}

async fn initialize_litep2p() -> (Litep2p, RequestResponseHandle) {
    let (config, handle) = RequestResponseConfig::new(
        litep2p::types::protocol::ProtocolName::from("/request/1"),
        2 * 256,
    );

    let litep2p = Litep2p::new(
        ConfigBuilder::new()
            .with_tcp(TcpConfig {
                listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
                yamux_config: Default::default(),
            })
            .with_request_response_protocol(config)
            .build(),
    )
    .await
    .unwrap();

    (litep2p, handle)
}

#[tokio::test]
async fn request_works() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, peer_store, requests) = initialize_libp2p();
    let (mut litep2p, mut handle) = initialize_litep2p().await;
    let address = litep2p.listen_addresses().next().unwrap().clone();
    let litep2p_peer = *litep2p.local_peer_id();
    let libp2p_peer = *libp2p.local_peer_id();
    let mut pending_responses = FuturesUnordered::new();

    tokio::spawn(peer_store.run());
    libp2p.dial(address).unwrap();

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::info!("Listening on {address:?}")
                    }
                    event => {
                        tracing::info!("libp2p: received {event:?}");
                    }
                }
            }
            event = litep2p.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { peer, .. } => {
                    // TODO: zzz
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    handle.send_request(peer, vec![0, 1, 2, 3, 4]).await.unwrap();
                }
                event => tracing::info!("litep2p: received {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                RequestResponseEvent::ResponseReceived {
                    peer,
                    request_id,
                    response,
                } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(response, vec![5, 6, 7, 8, 9]);
                    assert_eq!(request_id, 0usize);
                }
                RequestResponseEvent::RequestReceived {
                    peer,
                    request_id,
                    request
                } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(request, vec![1, 3, 3, 7]);
                    handle.send_response(request_id, vec![1, 3, 3, 8]).await.unwrap();
                }
                event => tracing::warn!("unhandle event: {event:?}"),
            },
            request = requests.recv() => match request {
                Ok(request) => {
                    request.pending_response.send(OutgoingResponse {
                        result: Ok(vec![5, 6, 7, 8, 9]),
                        reputation_changes: Vec::new(),
                        sent_feedback: None
                    }).unwrap();

                    let (tx, rx) = oneshot::channel();
                    pending_responses.push(rx);

                    libp2p.behaviour_mut().send_request(
                        &libp2p::PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap(),
                        "/request/1",
                        vec![1, 3, 3, 7],
                        tx,
                        IfDisconnected::ImmediateError,
                    );
                }
                Err(error) => {
                    tracing::error!("failed to read reqeuest: {error:?}")
                }
            },
            event = pending_responses.select_next_some(), if !pending_responses.is_empty() => {
                match event {
                    Ok(response) => {
                        assert_eq!(response.unwrap(), vec![1, 3, 3, 8]);
                        break
                    }
                    Err(error) => panic!("failed to receive response from peer: {error:?}"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                panic!("failed to receive request in time");
            }
        }
    }
}

#[tokio::test]
async fn substrate_reject_request() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, peer_store, requests) = initialize_libp2p();
    let (mut litep2p, mut handle) = initialize_litep2p().await;
    let address = litep2p.listen_addresses().next().unwrap().clone();
    let libp2p_peer = *libp2p.local_peer_id();

    tokio::spawn(peer_store.run());
    libp2p.dial(address).unwrap();

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::info!("Listening on {address:?}")
                    }
                    event => {
                        tracing::info!("libp2p: received {event:?}");
                    }
                }
            }
            event = litep2p.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { peer, .. } => {
                    // TODO: zzz
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    handle.send_request(peer, vec![0, 1, 2, 3, 4]).await.unwrap();
                }
                event => tracing::info!("litep2p: received {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                RequestResponseEvent::RequestFailed { peer, error, .. } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(error, RequestResponseError::Rejected);
                    break;
                }
                event => tracing::warn!("unhandle event: {event:?}"),
            },
            request = requests.recv() => match request {
                Ok(request) => {
                    drop(request);
                }
                Err(error) => {
                    tracing::error!("failed to read reqeuest: {error:?}")
                }
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                panic!("failed to receive request in time");
            }
        }
    }
}

#[tokio::test]
async fn litep2p_reject_request() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, peer_store, _) = initialize_libp2p();
    let (mut litep2p, mut handle) = initialize_litep2p().await;
    let address = litep2p.listen_addresses().next().unwrap().clone();
    let litep2p_peer = *litep2p.local_peer_id();
    let mut pending_responses = FuturesUnordered::new();

    tokio::spawn(peer_store.run());
    libp2p.dial(address).unwrap();

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::info!("Listening on {address:?}")
                    }
                    SwarmEvent::ConnectionEstablished { .. } => {
                        let (tx, rx) = oneshot::channel();
                        pending_responses.push(rx);

                        libp2p.behaviour_mut().send_request(
                            &libp2p::PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap(),
                            "/request/1",
                            vec![1, 3, 3, 7],
                            tx,
                            IfDisconnected::ImmediateError,
                        );
                    }
                    event => {
                        tracing::info!("libp2p: received {event:?}");
                    }
                }
            }
            event = litep2p.next_event() => match event.unwrap() {
                event => tracing::info!("litep2p: received {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                RequestResponseEvent::RequestReceived {
                    request_id,
                    ..
                } => {
                    handle.reject_request(request_id).await;
                }
                event => tracing::warn!("unhandle event: {event:?}"),
            },
            event = pending_responses.select_next_some(), if !pending_responses.is_empty() => {
                match event {
                    Ok(response) => {
                        assert!(std::matches!(response, Err(RequestFailure::Refused)));
                        break
                    }
                    Err(_) => panic!("failed to read response"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                panic!("failed to receive request in time");
            }
        }
    }
}

#[tokio::test]
async fn substrate_request_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, peer_store, requests) = initialize_libp2p();
    let (mut litep2p, mut handle) = initialize_litep2p().await;
    let address = litep2p.listen_addresses().next().unwrap().clone();
    let libp2p_peer = *libp2p.local_peer_id();
    let mut _timeout_request = None;

    tokio::spawn(peer_store.run());
    libp2p.dial(address).unwrap();

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::info!("Listening on {address:?}")
                    }
                    event => {
                        tracing::info!("libp2p: received {event:?}");
                    }
                }
            }
            event = litep2p.next_event() => match event.unwrap() {
                Litep2pEvent::ConnectionEstablished { peer, .. } => {
                    // TODO: zzz
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    handle.send_request(peer, vec![0, 1, 2, 3, 4]).await.unwrap();
                }
                event => tracing::info!("litep2p: received {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                RequestResponseEvent::RequestFailed { peer, error, .. } => {
                    assert_eq!(peer.to_bytes(), libp2p_peer.to_bytes());
                    assert_eq!(error, RequestResponseError::Timeout);
                    break;
                }
                event => tracing::warn!("unhandle event: {event:?}"),
            },
            request = requests.recv() => match request {
                Ok(request) => {
                    _timeout_request = Some(request);
                }
                Err(error) => {
                    tracing::error!("failed to read reqeuest: {error:?}")
                }
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                panic!("failed to receive request in time");
            }
        }
    }
}

#[tokio::test]
async fn litep2p_request_timeout() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut libp2p, peer_store, _) = initialize_libp2p();
    let (mut litep2p, mut handle) = initialize_litep2p().await;
    let address = litep2p.listen_addresses().next().unwrap().clone();
    let litep2p_peer = *litep2p.local_peer_id();
    let mut pending_responses = FuturesUnordered::new();

    tokio::spawn(peer_store.run());
    libp2p.dial(address).unwrap();

    loop {
        tokio::select! {
            event = libp2p.select_next_some() => {
                match event {
                    SwarmEvent::NewListenAddr { address, .. } => {
                        tracing::info!("Listening on {address:?}")
                    }
                    SwarmEvent::ConnectionEstablished { .. } => {
                        let (tx, rx) = oneshot::channel();
                        pending_responses.push(rx);

                        libp2p.behaviour_mut().send_request(
                            &libp2p::PeerId::from_bytes(&litep2p_peer.to_bytes()).unwrap(),
                            "/request/1",
                            vec![1, 3, 3, 7],
                            tx,
                            IfDisconnected::ImmediateError,
                        );
                    }
                    event => {
                        tracing::info!("libp2p: received {event:?}");
                    }
                }
            }
            event = litep2p.next_event() => match event.unwrap() {
                event => tracing::info!("litep2p: received {event:?}"),
            },
            event = handle.next() => match event.unwrap() {
                RequestResponseEvent::RequestReceived { .. } => {},
                event => tracing::warn!("unhandle event: {event:?}"),
            },
            event = pending_responses.select_next_some(), if !pending_responses.is_empty() => {
                match event {
                    Ok(response) => {
                        assert!(std::matches!(response, Err(RequestFailure::Network(OutboundFailure::Timeout))));
                        break
                    }
                    Err(_) => panic!("failed to read response"),
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                panic!("failed to receive request in time");
            }
        }
    }
}
