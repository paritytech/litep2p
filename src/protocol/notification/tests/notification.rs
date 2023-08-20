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

// TODO: remove
#![allow(unused)]

use crate::{
    error::{Error, SubstreamError},
    mock::substream::MockSubstream,
    peer_id::PeerId,
    protocol::{
        notification::{
            negotiation::HandshakeEvent,
            tests::{add_peer, make_notification_protocol},
            types::{
                NotificationError, NotificationEvent, ValidationResult, ASYNC_CHANNEL_SIZE,
                SYNC_CHANNEL_SIZE,
            },
            InboundState, OutboundState, PeerContext, PeerState,
        },
        ProtocolCommand,
    },
    types::protocol::ProtocolName,
};

use bytes::BytesMut;
use futures::StreamExt;

use std::task::Poll;

#[tokio::test]
async fn sync_notifications_clogged() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut notif, mut handle, _sender) = make_notification_protocol();
    let (peer, service, mut receiver) = add_peer();

    notif.peers.insert(
        peer,
        PeerContext {
            service: (),
            state: PeerState::Validating {
                protocol: ProtocolName::from("/notif/1"),
                outbound: OutboundState::Open {
                    handshake: vec![1, 2, 3, 4],
                    outbound: Box::new(MockSubstream::new()),
                },
                inbound: InboundState::Accepting,
            },
        },
    );

    notif
        .on_negotiation_event(
            peer,
            HandshakeEvent::InboundAccepted {
                peer,
                substream: Box::new(MockSubstream::new()),
            },
        )
        .await;

    assert_eq!(
        handle.next_event().await.unwrap(),
        NotificationEvent::NotificationStreamOpened {
            protocol: ProtocolName::from("/notif/1"),
            peer,
            handshake: vec![1, 2, 3, 4]
        },
    );

    for i in 0..SYNC_CHANNEL_SIZE {
        handle
            .send_sync_notification(peer, vec![1, 3, 3, 7])
            .unwrap();
    }

    // try to send one more notification and verify that the call would block
    assert_eq!(
        handle.send_sync_notification(peer, vec![1, 3, 3, 9]),
        Err(NotificationError::ChannelClogged)
    );
}

#[tokio::test]
async fn async_notifications_clogged() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut notif, mut handle, _sender) = make_notification_protocol();
    let (peer, service, mut receiver) = add_peer();

    notif.peers.insert(
        peer,
        PeerContext {
            service: (),
            state: PeerState::Validating {
                protocol: ProtocolName::from("/notif/1"),
                outbound: OutboundState::Open {
                    handshake: vec![1, 2, 3, 4],
                    outbound: Box::new(MockSubstream::new()),
                },
                inbound: InboundState::Accepting,
            },
        },
    );

    notif
        .on_negotiation_event(
            peer,
            HandshakeEvent::InboundAccepted {
                peer,
                substream: Box::new(MockSubstream::new()),
            },
        )
        .await;

    assert_eq!(
        handle.next_event().await.unwrap(),
        NotificationEvent::NotificationStreamOpened {
            protocol: ProtocolName::from("/notif/1"),
            peer,
            handshake: vec![1, 2, 3, 4]
        },
    );

    for i in 0..ASYNC_CHANNEL_SIZE {
        handle
            .send_async_notification(peer, vec![1, 3, 3, 7])
            .await
            .unwrap();
    }

    // try to send one more notification and verify that the call would block
    assert!(futures::poll!(Box::pin(
        handle.send_async_notification(peer, vec![1, 3, 3, 9])
    ))
    .is_pending());

    // poll one async notification from the queue
    let _ = notif.receivers.next().await.unwrap();

    // try to send the notification again and verify that this time it works
    assert!(futures::poll!(Box::pin(
        handle.send_async_notification(peer, vec![1, 3, 3, 9])
    ))
    .is_ready());
}
