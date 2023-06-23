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
    protocol::request_response::types::{Config as RequestResponseConfig, RequestResponseEvent},
    transport::tcp::config::TransportConfig as TcpTransportConfig,
    types::protocol::ProtocolName,
    Litep2p, Litep2pEvent,
};

async fn connect_peers(litep2p1: &mut Litep2p, litep2p2: &mut Litep2p) {
    let address = litep2p2.listen_addresses().next().unwrap().clone();
    litep2p1.connect(address).unwrap();

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

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
}

#[tokio::test]
async fn send_request_receive_response() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (req_resp_config1, mut handle1) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64);
    let config1 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
        })
        .with_request_response_protocol(req_resp_config1)
        .build();

    let (req_resp_config2, mut handle2) =
        RequestResponseConfig::new(ProtocolName::from("/protocol/1"), 64);
    let config2 = Litep2pConfigBuilder::new()
        .with_keypair(Keypair::generate())
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
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
        handle2.next_event().await.unwrap(),
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
        handle1.next_event().await.unwrap(),
        RequestResponseEvent::ResponseReceived {
            peer: peer2,
            request_id,
            response: vec![1, 3, 3, 8],
        }
    );
}
