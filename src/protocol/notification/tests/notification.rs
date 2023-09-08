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
    mock::substream::{DummySubstream, MockSubstream},
    protocol::{
        connection::ConnectionHandle,
        notification::{
            tests::make_notification_protocol,
            types::{NotificationError, NotificationEvent},
            InboundState, NotificationProtocol, OutboundState, PeerContext, PeerState,
        },
        Direction, InnerTransportEvent, ProtocolCommand,
    },
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId,
};

use futures::StreamExt;
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::task::Poll;

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

fn next_outbound_state(state: usize) -> OutboundState {
    match state {
        0 => OutboundState::Closed,
        1 => OutboundState::OutboundInitiated {
            substream: SubstreamId::new(),
        },
        2 => OutboundState::Negotiating,
        3 => OutboundState::Open {
            handshake: vec![1, 3, 3, 7],
            outbound: Box::new(MockSubstream::new()),
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

// register new connection to `NotificationProtocol`
async fn register_peer(
    notif: &mut NotificationProtocol,
    sender: &mut Sender<InnerTransportEvent>,
) -> (PeerId, Receiver<ProtocolCommand>) {
    let peer = PeerId::random();
    let (conn_tx, conn_rx) = channel(64);

    sender
        .send(InnerTransportEvent::ConnectionEstablished {
            peer,
            connection: ConnectionId::new(),
            address: Multiaddr::empty(),
            sender: ConnectionHandle::new(conn_tx),
        })
        .await
        .unwrap();

    // poll the protocol to register the peer
    notif.next_event().await;

    assert!(std::matches!(
        notif.peers.get(&peer),
        Some(PeerContext {
            state: PeerState::Closed { .. }
        })
    ));

    (peer, conn_rx)
}

#[tokio::test]
async fn open_substream_connection_closed() {
    open_substream(
        PeerState::Closed {
            _pending_open: None,
        },
        true,
    )
    .await;
}

#[tokio::test]
async fn open_substream_already_initiated() {
    open_substream(
        PeerState::OutboundInitiated {
            substream: SubstreamId::new(),
        },
        false,
    )
    .await;
}

#[tokio::test]
async fn open_substream_already_open() {
    open_substream(
        PeerState::Open {
            outbound: Box::new(MockSubstream::new()),
        },
        false,
    )
    .await;
}

#[tokio::test]
async fn open_substream_under_validation() {
    for i in 0..6 {
        for k in 0..4 {
            open_substream(
                PeerState::Validating {
                    protocol: ProtocolName::from("/notif/1"),
                    fallback: None,
                    outbound: next_outbound_state(k),
                    inbound: next_inbound_state(i),
                },
                false,
            )
            .await;
        }
    }
}

async fn open_substream(state: PeerState, succeeds: bool) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut notif, _handle, _sender, mut tx) = make_notification_protocol();
    let (peer, mut receiver) = register_peer(&mut notif, &mut tx).await;

    let context = notif.peers.get_mut(&peer).unwrap();
    context.state = state;

    notif.on_open_substream(peer).await.unwrap();
    assert!(receiver.try_recv().is_ok() == succeeds);
}

#[tokio::test]
async fn open_substream_no_connection() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut notif, _handle, _sender, _tx) = make_notification_protocol();
    assert!(notif.on_open_substream(PeerId::random()).await.is_err());
}

#[tokio::test]
async fn remote_opens_multiple_inbound_substreams() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let protocol = ProtocolName::from("/notif/1");
    let (mut notif, _handle, _sender, mut tx) = make_notification_protocol();
    let (peer, _receiver) = register_peer(&mut notif, &mut tx).await;

    // // open substream, poll the result and verify that the peer is in correct state
    tx.send(InnerTransportEvent::SubstreamOpened {
        peer,
        protocol: protocol.clone(),
        fallback: None,
        direction: Direction::Inbound,
        substream: Box::new(DummySubstream::new()),
    })
    .await
    .unwrap();
    notif.next_event().await;

    match notif.peers.get(&peer) {
        Some(PeerContext {
            state:
                PeerState::Validating {
                    protocol,
                    fallback: None,
                    outbound: OutboundState::Closed,
                    inbound: InboundState::ReadingHandshake,
                },
        }) => {
            assert_eq!(protocol, &ProtocolName::from("/notif/1"));
        }
        state => panic!("invalid state: {state:?}"),
    }

    // try to open another substream and verify it's discarded and the state is otherwise preserved
    let mut substream = MockSubstream::new();
    substream.expect_poll_close().times(1).return_once(|_| Poll::Ready(Ok(())));

    tx.send(InnerTransportEvent::SubstreamOpened {
        peer,
        protocol: protocol.clone(),
        fallback: None,
        direction: Direction::Inbound,
        substream: Box::new(substream),
    })
    .await
    .unwrap();
    notif.next_event().await;

    match notif.peers.get(&peer) {
        Some(PeerContext {
            state:
                PeerState::Validating {
                    protocol,
                    fallback: None,
                    outbound: OutboundState::Closed,
                    inbound: InboundState::ReadingHandshake,
                },
        }) => {
            assert_eq!(protocol, &ProtocolName::from("/notif/1"));
        }
        state => panic!("invalid state: {state:?}"),
    }
}
