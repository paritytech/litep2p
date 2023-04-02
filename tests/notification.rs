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

use litep2p::{config::LiteP2pConfiguration, Litep2p, Litep2pEvent};
use multiaddr::Multiaddr;

#[tokio::test]
async fn notification_substream() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init()
        .expect("to succeed");

    let addr1: Multiaddr = "/ip6/::1/tcp/8888".parse().expect("valid multiaddress");
    let mut litep2p1 = Litep2p::new(LiteP2pConfiguration::new(vec![addr1.clone()], vec![]))
        .await
        .unwrap();

    let addr2: Multiaddr = "/ip6/::1/tcp/8889".parse().expect("valid multiaddress");
    let mut litep2p2 = Litep2p::new(LiteP2pConfiguration::new(vec![addr2.clone()], vec![]))
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

    // attempt to open substream to remote peer.
    // litep2p1
    //     .open_notification_substream("/notif/1".to_owned(), peer2, vec![1, 3, 3, 7])
    //     .await
    //     .unwrap();

    // match litep2p2.next_event().await {
    //     Ok(Litep2pEvent::InboundSubstream(protocol, peer, handshake, tx)) => {
    //         assert_eq!(&protocol, "/notif/1");
    //         assert_eq!(peer, peer1);
    //         assert_eq!(handshake, vec![1, 3, 3, 7]);
    //         tx.send(ValidationResult::Accept).unwrap();
    //     }
    //     event => panic!("invalid event received {event:?}"),
    // }

    // // verify that both get the "substream opened" event
    // let (_sink2, _sink1) = match tokio::join!(litep2p1.next_event(), litep2p2.next_event()) {
    //     (
    //         Ok(Litep2pEvent::SubstreamOpened(protocol2, sub_peer2, sink2)),
    //         Ok(Litep2pEvent::SubstreamOpened(protocol1, sub_peer1, sink1)),
    //     ) => {
    //         assert!(protocol1 == protocol2 && &protocol1 == "/notif/1");
    //         assert_eq!(sub_peer1, peer1);
    //         assert_eq!(sub_peer2, peer2);
    //         (sink2, sink1)
    //     }
    //     _ => panic!("invalid event"),
    // };
}
