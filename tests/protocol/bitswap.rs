// Copyright 2025 litep2p developers
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

use futures::stream::StreamExt;
use litep2p::{
    config::ConfigBuilder,
    protocol::libp2p::bitswap::{
        BitswapEvent, BitswapHandle, BlockPresenceType, Config as BitswapConfig, ResponseType,
        WantType,
    },
    transport::tcp::config::Config as TcpConfig,
    types::{cid::Cid, multihash::Code},
    Litep2p,
};
use multihash::MultihashDigest;
use std::{pin::pin, time::Duration};
use tracing::debug;

fn make_litep2p() -> (Litep2p, BitswapHandle) {
    let (bitswap_config, bitswap) = BitswapConfig::new();

    let litep2p = Litep2p::new(
        ConfigBuilder::new()
            .with_tcp(TcpConfig {
                listen_addresses: vec!["/ip6/::1/tcp/0".parse().unwrap()],
                nodelay: true,
                ..Default::default()
            })
            .with_libp2p_bitswap(bitswap_config)
            .build(),
    )
    .unwrap();

    (litep2p, bitswap)
}

#[tokio::test]
async fn bitswap_e2e() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut litep2p_client, mut bitswap_client) = make_litep2p();
    let (mut litep2p_server, mut bitswap_server) = make_litep2p();

    // Checking maximum block size of 2 MiB, as per spec.
    let payload = vec![0xde; 2 * 1024 * 1024];
    let multihash = Code::Sha2_256.digest(&payload);
    let multihash = cid::multihash::Multihash::wrap(multihash.code(), multihash.digest()).unwrap();
    let cid = Cid::new_v1(0x55, multihash);

    litep2p_client.add_known_address(
        *litep2p_server.local_peer_id(),
        litep2p_server.listen_addresses().map(Clone::clone),
    );
    bitswap_client
        .send_request(*litep2p_server.local_peer_id(), vec![(cid, WantType::Have)])
        .await;

    let mut timeout = pin!(tokio::time::sleep(Duration::from_secs(10)));

    loop {
        tokio::select! {
            () = &mut timeout => {
                panic!("test timed out");
            },
            _ = litep2p_client.next_event() => {},
            _ = litep2p_server.next_event() => {},
            event = bitswap_client.next() => {
                match event {
                    Some(BitswapEvent::Response { peer, responses }) => {
                        assert_eq!(&peer, litep2p_server.local_peer_id());
                        assert_eq!(responses.len(), 1);

                        match responses.first().unwrap() {
                            ResponseType::Presence { cid: received_cid, presence } => {
                                if received_cid != &cid {
                                    panic!(
                                        "got unexpected 'have' CID from remote: {}",
                                        received_cid,
                                    );
                                }
                                if !matches!(presence, BlockPresenceType::Have) {
                                    panic!(
                                        "remote doesn't have the requested CID",
                                    );
                                }
                                debug!("'Have' response received");
                                bitswap_client
                                    .send_request(peer, vec![(cid, WantType::Block)])
                                    .await;
                            },
                            ResponseType::Block { cid: received_cid, block } => {
                                if received_cid != &cid {
                                    panic!(
                                        "got unexpected 'block' CID from remote: {}",
                                        received_cid,
                                    );
                                }
                                assert_eq!(block, &payload);
                                debug!("'Block' response received");
                                break;
                            }
                        }
                    },
                    _ => panic!("unexppected bitswap client event"),
                }
            },
            event = bitswap_server.next() => {
                match event {
                    Some(BitswapEvent::Request { peer, cids }) => {
                        assert_eq!(&peer, litep2p_client.local_peer_id());
                        assert_eq!(cids.len(), 1);

                        let (got_cid, want_type) = cids.first().unwrap();
                        assert_eq!(got_cid, &cid);

                        match want_type {
                            WantType::Have => {
                                debug!("'Have' request received");
                                bitswap_server.send_response(peer, vec![ResponseType::Presence {
                                    cid: cid.clone(),
                                    presence: BlockPresenceType::Have,
                                }])
                                .await;
                            },
                            WantType::Block => {
                                debug!("'Block' request received");
                                bitswap_server.send_response(peer, vec![ResponseType::Block {
                                    cid: cid.clone(),
                                    block: payload.clone(),
                                }])
                                .await;
                            },
                        }

                    }
                    _ => panic!("unexpected bitswap server event"),
                }
            }
        }
    }
}
