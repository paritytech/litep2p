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

//! This example demonstrates a simple echo server using the notification protocol
//! in which client connects to server and sends a message to server every 3 seconds

use futures::StreamExt;
use litep2p::{
    config::Litep2pConfigBuilder,
    protocol::notification::{
        NotificationConfigBuilder, NotificationEvent, NotificationHandle, ValidationResult,
    },
    transport::quic::config::TransportConfig as QuicTransportConfig,
    types::protocol::ProtocolName,
    Litep2p, Litep2pEvent,
};
use multiaddr::Multiaddr;
use std::time::Duration;

// async loop for the client
async fn client_event_loop(
    mut litep2p: Litep2p,
    mut handle: NotificationHandle,
    address: Multiaddr,
) {
    // connect to server and open substream to them over the echo protocol
    litep2p.dial_address(address).await.unwrap();

    match litep2p.next_event().await.unwrap() {
        Litep2pEvent::ConnectionEstablished { peer, .. } => {
            handle.open_substream(peer).await.unwrap();
        }
        _ => panic!("unexpected event received"),
    }

    // wait until the substream is opened
    //
    // note that both client and server must validate the substream as the notification protocol
    // opens two bidirectional substreams. For client this can simply be an ack without validating
    // the protocol/handshake but for more complex protocol implementations it may be necessary have
    // validation in both ends.
    //
    // currently it's not possible to automatically accept inbound substreams but that is on the roadmap
    let peer = loop {
        tokio::select! {
            _ = litep2p.next_event() => {}
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { peer, .. } => {
                    handle.send_validation_result(peer, ValidationResult::Accept).await;
                }
                NotificationEvent::NotificationStreamOpened { peer, .. } => break peer,
                _ => {},
            }
        }
    };

    // after the substream is open, send notification to server and print the response to stdout
    loop {
        tokio::select! {
            _ = litep2p.next_event() => {}
            event = handle.next() => match event.unwrap() {
                NotificationEvent::NotificationReceived { peer, notification } => {
                    println!("received response from server ({peer:?}): {notification:?}");
                }
                _ => {},
            },
            _ = tokio::time::sleep(Duration::from_secs(3)) => {
                handle.send_sync_notification(peer, vec![1, 3, 3, 7]).unwrap();
            }
        }
    }
}

/// event loop for the server
async fn server_event_loop(mut litep2p: Litep2p, mut handle: NotificationHandle) {
    loop {
        tokio::select! {
            _ = litep2p.next_event() => {}
            event = handle.next() => match event.unwrap() {
                NotificationEvent::ValidateSubstream { peer, .. } => {
                    handle.send_validation_result(peer, ValidationResult::Accept).await;
                }
                NotificationEvent::NotificationReceived { peer, notification } => {
                    handle.send_async_notification(peer, notification).await.unwrap();
                }
                _ => {},
            },
        }
    }
}

#[tokio::main]
async fn main() {
    // build notification config and handle for the first peer
    let (echo_config1, echo_handle1) =
        NotificationConfigBuilder::new(ProtocolName::from("/echo/1"))
            .with_max_size(256)
            .with_handshake(vec![1, 3, 3, 7])
            .build();

    // build notification config and handle for the second peer
    let (echo_config2, echo_handle2) =
        NotificationConfigBuilder::new(ProtocolName::from("/echo/1"))
            .with_max_size(256)
            .with_handshake(vec![1, 3, 3, 7])
            .build();

    // build `Litep2p` for the first peer
    let litep2p1 = Litep2p::new(
        Litep2pConfigBuilder::new()
            .with_quic(QuicTransportConfig {
                listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            })
            .with_notification_protocol(echo_config1)
            .build(),
    )
    .await
    .unwrap();

    // build `Litep2p` for the second peer
    let litep2p2 = Litep2p::new(
        Litep2pConfigBuilder::new()
            .with_quic(QuicTransportConfig {
                listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
            })
            .with_notification_protocol(echo_config2)
            .build(),
    )
    .await
    .unwrap();
    let listen_address = litep2p2.listen_addresses().next().unwrap().clone();

    tokio::spawn(client_event_loop(litep2p1, echo_handle1, listen_address));
    tokio::spawn(server_event_loop(litep2p2, echo_handle2));

    loop {
        tokio::time::sleep(Duration::from_secs(10)).await;
    }
}
