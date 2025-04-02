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

use futures::StreamExt;
use litep2p::{
    config::ConfigBuilder,
    crypto::ed25519::SecretKey,
    protocol::{
        libp2p::{
            bitswap::{BitswapCommand, BitswapHandle, Config as BitswapConfig},
            kademlia::{ConfigBuilder as KadConfigBuilder, KademliaCommand, KademliaHandle},
        },
        notification::{
            ConfigBuilder as NotificationConfigBuilder, NotificationCommand, NotificationHandle,
        },
        request_response::{
            ConfigBuilder as RequestResponseConfigBuilder, RequestResponseCommand,
            RequestResponseHandle,
        },
    },
    transport::tcp::config::Config as TcpConfig,
    Litep2p, ProtocolName,
};

const NUM_WORKER_THREADS: usize = 32;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub enum FuzzMessage {
    RequestResponse(RequestResponseCommand),
    Kademlia(KademliaCommand),
    Notification(NotificationCommand),
    Bitswap(BitswapCommand),
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct FuzzData {
    pub data: Vec<(u8, FuzzMessage)>,
}

fn main() {
    ziggy::fuzz!(|data: &[u8]| {
        let data = data;
        let Ok(mut data) = bincode::deserialize::<FuzzData>(data) else {
            return;
        };
        tokio::runtime::Builder::new_current_thread()
            .worker_threads(NUM_WORKER_THREADS)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let (
                    mut litep2p1,
                    mut kad_handle1,
                    mut bitswap_handle1,
                    mut rr_handle1,
                    mut notif_handle1,
                ) = create_instance(&mut [0u8; 32]);
                let (
                    mut litep2p2,
                    mut kad_handle2,
                    mut bitswap_handle2,
                    mut rr_handle2,
                    mut notif_handle2,
                ) = create_instance(&mut [1u8; 32]);
                let address = litep2p2.listen_addresses().next().unwrap().clone();
                litep2p1.dial_address(address).await.unwrap();
                loop {
                    if let Some((peer, message)) = data.data.pop() {
                        let handles = if peer % 2 == 0 {
                            (
                                &mut kad_handle1,
                                &mut bitswap_handle1,
                                &mut rr_handle1,
                                &mut notif_handle1,
                                &mut litep2p1,
                            )
                        } else {
                            (
                                &mut kad_handle2,
                                &mut bitswap_handle2,
                                &mut rr_handle2,
                                &mut notif_handle2,
                                &mut litep2p2,
                            )
                        };
                        match message {
                            FuzzMessage::Kademlia(message) => {
                                let _ = handles.0.add_known_peer(*handles.4.local_peer_id(), vec![handles.4.listen_addresses().next().unwrap().clone()]).await;
                                let _ = handles.0.fuzz_send_message(message).await;
                            }
                            FuzzMessage::Bitswap(message) => {
                                let _ = handles.1.fuzz_send_message(message).await;
                            }
                            FuzzMessage::RequestResponse(message) => {
                                let _ = handles.2.fuzz_send_message(message).await;
                            }
                            FuzzMessage::Notification(message) => {
                                let _ = handles.3.fuzz_send_message(message).await;
                            }
                        };
                    };
                    tokio::select! {
                        _event = litep2p1.next_event() => {},
                        _event = litep2p2.next_event() => {},
                        _event = rr_handle1.next() => {},
                        _event = rr_handle2.next() => {},
                        _event = kad_handle1.next() => {},
                        _event = kad_handle2.next() => {},
                        _event = bitswap_handle1.next() => {},
                        _event = bitswap_handle2.next() => {},
                        _event = notif_handle1.next() => {},
                        _event = notif_handle2.next() => {},
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
