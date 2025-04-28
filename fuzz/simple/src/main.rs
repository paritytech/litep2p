// Copyright 2025 Security Research Labs GmbH
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

mod protocol;
use futures::StreamExt;
use litep2p::{
    Litep2p, ProtocolName,
    config::ConfigBuilder,
    crypto::ed25519::SecretKey,
    protocol::{
        libp2p::{
            bitswap::{BitswapHandle, Config as BitswapConfig},
            kademlia::{ConfigBuilder as KadConfigBuilder, KademliaHandle},
        },
        notification::{ConfigBuilder as NotificationConfigBuilder, NotificationHandle},
        request_response::{ConfigBuilder as RequestResponseConfigBuilder, RequestResponseHandle},
    },
    transport::tcp::config::Config as TcpConfig,
};
use protocol::{FuzzProtocol, FuzzProtocolHandle};

const NUM_WORKER_THREADS: usize = 32;
const NUM_PROTOCOLS: u8 = 4;

fn main() {
    ziggy::fuzz!(|data: &[u8]| {
        if data.len() < 2 {
            return;
        };
        let protocol = data[0] % NUM_PROTOCOLS;
        tokio::runtime::Builder::new_current_thread()
            .worker_threads(NUM_WORKER_THREADS)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (mut litep2p1, _kad_handle1, _bitswap_handle1, _rr_handle1, _notif_handle1) =
                    create_instance(&mut [0u8; 32]);
                let (
                    mut litep2p2,
                    mut kad_handle2,
                    mut bitswap_handle2,
                    mut rr_handle2,
                    mut notif_handle2,
                ) = create_instance_fuzz(&mut [1u8; 32]);
                let peer = *litep2p1.local_peer_id();
                let address = litep2p2.listen_addresses().next().unwrap().clone();
                litep2p1.dial_address(address).await.unwrap();
                match protocol {
                    0 => kad_handle2.send_message(peer, data[1..].to_vec()).await,
                    1 => bitswap_handle2.send_message(peer, data[1..].to_vec()).await,
                    2 => rr_handle2.send_message(peer, data[1..].to_vec()).await,
                    3 => notif_handle2.send_message(peer, data[1..].to_vec()).await,
                    _ => unreachable!(),
                }
                loop {
                    tokio::select! {
                        _event = litep2p1.next_event() => {},
                        _event = litep2p2.next_event() => {},
                    }
                    if tokio::runtime::Handle::current().metrics().num_alive_tasks() > 6 {
                        return;
                    }
                }
            });
    });
}

fn create_instance(
    key: &mut [u8; 32],
) -> (
    Litep2p,
    KademliaHandle,
    BitswapHandle,
    RequestResponseHandle,
    NotificationHandle,
) {
    let (kad_config, kad_handle) = KadConfigBuilder::new()
        .with_protocol_names(vec![ProtocolName::Allocated("/ksmcc3/kad".into())])
        .build();
    let (bitswap_config, bitswap_handle) = BitswapConfig::new();
    let (rr_config, rr_handle) =
        RequestResponseConfigBuilder::new(ProtocolName::Allocated("/ksmcc3/rr".into()))
            .with_max_size(1024 * 1024)
            .build();
    let (_notif_config, notif_handle) =
        NotificationConfigBuilder::new(ProtocolName::Allocated("/ksmcc3/notif".into()))
            .with_max_size(1024 * 1024)
            .with_handshake("fuzz".as_bytes().to_vec())
            .build();
    let config = ConfigBuilder::new()
        .with_libp2p_kademlia(kad_config)
        .with_request_response_protocol(rr_config)
        .with_libp2p_bitswap(bitswap_config)
        .with_tcp(TcpConfig::default())
        .with_keypair(SecretKey::try_from_bytes(key).unwrap().into())
        .build();
    (
        Litep2p::new(config).unwrap(),
        kad_handle,
        bitswap_handle,
        rr_handle,
        notif_handle,
    )
}

fn create_instance_fuzz(
    key: &mut [u8; 32],
) -> (
    Litep2p,
    FuzzProtocolHandle,
    FuzzProtocolHandle,
    FuzzProtocolHandle,
    FuzzProtocolHandle,
) {
    let (custom_kad, kad_handle) = FuzzProtocol::new("/kscmcc3/kad");
    let (custom_notif, bitswap_handle) = FuzzProtocol::new("/ipfs/bitswap/1.2.0");
    let (custom_rr, rr_handle) = FuzzProtocol::new("/kscmcc3/rr");
    let (custom_bitswap, notif_handle) = FuzzProtocol::new("/kscmcc3/notif");
    let config = ConfigBuilder::new()
        .with_user_protocol(Box::new(custom_kad))
        .with_user_protocol(Box::new(custom_notif))
        .with_user_protocol(Box::new(custom_rr))
        .with_user_protocol(Box::new(custom_bitswap))
        .with_tcp(TcpConfig::default())
        .with_keypair(SecretKey::try_from_bytes(key).unwrap().into())
        .build();
    (
        Litep2p::new(config).unwrap(),
        kad_handle,
        bitswap_handle,
        rr_handle,
        notif_handle,
    )
}
