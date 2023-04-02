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

#![allow(unused)]

use litep2p::{
    config::LiteP2pConfiguration,
    protocol::request_response::{RequestResponseEvent, RequestResponseService},
    Litep2p, Litep2pEvent,
};
use multiaddr::Multiaddr;

#[tokio::test]
async fn request_response() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .expect("to succeed");

    use litep2p::protocol::request_response::RequestResponseProtocolConfig;

    let (config1, mut service1) = RequestResponseProtocolConfig::new("/request/1".to_owned(), None);
    let (config2, mut service2) = RequestResponseProtocolConfig::new("/request/1".to_owned(), None);

    let addr1: Multiaddr = "/ip6/::1/tcp/8888".parse().expect("valid multiaddress");
    let mut litep2p1 = Litep2p::new(LiteP2pConfiguration::new(
        vec![addr1.clone()],
        vec![config1],
    ))
    .await
    .unwrap();

    let addr2: Multiaddr = "/ip6/::1/tcp/8889".parse().expect("valid multiaddress");
    let mut litep2p2 = Litep2p::new(LiteP2pConfiguration::new(
        vec![addr2.clone()],
        vec![config2],
    ))
    .await
    .unwrap();

    // attempt to open connection to `litep2p` and verify that both got the event
    litep2p1.open_connection(addr2).await;

    let (peer1, peer2) = match tokio::join!(litep2p1.next_event(), litep2p2.next_event()) {
        (
            Ok(Litep2pEvent::ConnectionEstablished(peer2)),
            Ok(Litep2pEvent::ConnectionEstablished(peer1)),
        ) => {
            assert_eq!(peer2, *litep2p2.local_peer_id());
            assert_eq!(peer1, *litep2p1.local_peer_id());
            (peer1, peer2)
        }
        _ => panic!("invalid event"),
    };

    // verify that both peers are identified by each other
    match tokio::join!(litep2p1.next_event(), litep2p2.next_event()) {
        (
            Ok(Litep2pEvent::PeerIdentified {
                peer: identified_peer2,
                supported_protocols: supported_protocols1,
            }),
            Ok(Litep2pEvent::PeerIdentified {
                peer: identified_peer1,
                supported_protocols: supported_protocols2,
            }),
        ) => {
            assert_eq!(peer1, identified_peer1);
            assert_eq!(peer2, identified_peer2);
            assert_eq!(supported_protocols1, supported_protocols2);
        }
        _ => panic!("invalid event"),
    }

    tokio::spawn(async move {
        service1.send_request(peer2, vec![1, 3, 3, 7]).await;

        while let Some(event) = service1.next_event().await {
            match event {
                RequestResponseEvent::RequestReceived {
                    peer,
                    request_id,
                    request,
                } => {
                    todo!();
                }
                RequestResponseEvent::ResponseReceived { request, response } => {
                    tracing::info!(?request, ?response, "response received");
                }
                RequestResponseEvent::RequestFailure { request, error } => {
                    todo!();
                }
            }
        }
    });

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {},
                _ = litep2p2.next_event() => {},
            }
        }
    });

    while let Some(event) = service2.next_event().await {
        match event {
            RequestResponseEvent::RequestReceived {
                peer,
                request_id,
                request,
            } => {
                service2
                    .send_response(request_id, vec![1, 3, 3, 8])
                    .await
                    .unwrap();
            }
            RequestResponseEvent::ResponseReceived { request, response } => {
                todo!();
            }
            RequestResponseEvent::RequestFailure { request, error } => {
                todo!();
            }
        }
    }
}
