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

use crate::{
    mock::substream::MockSubstream,
    protocol::notification::{
        tests::make_notification_protocol,
        types::{NotificationError, NotificationEvent},
        InboundState, OutboundState, PeerContext, PeerState,
    },
    types::{protocol::ProtocolName, SubstreamId},
    PeerId,
};

use futures::StreamExt;

fn next_inbound_state(state: usize) -> InboundState {
    match state {
        0 => InboundState::Closed,
        1 => InboundState::ReadingHandshake,
        2 => InboundState::Validating {
            inbound: Box::new(MockSubstream::new()),
        },
        3 => InboundState::SendingHandshake,
        4 => InboundState::_Accepting,
        5 => InboundState::Open {
            inbound: Box::new(MockSubstream::new()),
        },
        _ => panic!(),
    }
}

#[tokio::test]
async fn connection_closed_for_outbound_open_substream() {
    let peer = PeerId::random();

    for i in 0..6 {
        connection_closed(
            peer,
            PeerState::Validating {
                protocol: ProtocolName::from("/notif/1"),
                fallback: None,
                outbound: OutboundState::Open {
                    handshake: vec![1, 2, 3, 4],
                    outbound: Box::new(MockSubstream::new()),
                },
                inbound: next_inbound_state(i),
            },
            Some(NotificationEvent::NotificationStreamOpenFailure {
                peer,
                error: NotificationError::Rejected,
            }),
        )
        .await;
    }
}

#[tokio::test]
async fn connection_closed_for_outbound_initiated_substream() {
    let peer = PeerId::random();

    for i in 0..6 {
        connection_closed(
            peer,
            PeerState::Validating {
                protocol: ProtocolName::from("/notif/1"),
                fallback: None,
                outbound: OutboundState::OutboundInitiated {
                    substream: SubstreamId::from(0usize),
                },
                inbound: next_inbound_state(i),
            },
            Some(NotificationEvent::NotificationStreamOpenFailure {
                peer,
                error: NotificationError::Rejected,
            }),
        )
        .await;
    }
}

#[tokio::test]
async fn connection_closed_for_outbound_negotiated_substream() {
    let peer = PeerId::random();

    for i in 0..6 {
        connection_closed(
            peer,
            PeerState::Validating {
                protocol: ProtocolName::from("/notif/1"),
                fallback: None,
                outbound: OutboundState::Negotiating,
                inbound: next_inbound_state(i),
            },
            Some(NotificationEvent::NotificationStreamOpenFailure {
                peer,
                error: NotificationError::Rejected,
            }),
        )
        .await;
    }
}

#[tokio::test]
async fn connection_closed_for_open_notification_stream() {
    let peer = PeerId::random();

    connection_closed(
        peer,
        PeerState::Open {
            outbound: Box::new(MockSubstream::new()),
        },
        Some(NotificationEvent::NotificationStreamClosed { peer }),
    )
    .await;
}

#[tokio::test]
async fn connection_closed_for_initiated_substream() {
    let peer = PeerId::random();

    connection_closed(
        peer,
        PeerState::OutboundInitiated {
            substream: SubstreamId::new(),
        },
        Some(NotificationEvent::NotificationStreamOpenFailure {
            peer,
            error: NotificationError::Rejected,
        }),
    )
    .await;
}

// inbound state is ignored
async fn connection_closed(peer: PeerId, state: PeerState, event: Option<NotificationEvent>) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut notif, mut handle, _sender, _tx) = make_notification_protocol();

    notif.peers.insert(peer, PeerContext { state });
    notif.on_connection_closed(peer).await.unwrap();

    if let Some(expected) = event {
        assert_eq!(handle.next().await.unwrap(), expected);
    }
    assert!(!notif.peers.contains_key(&peer))
}
