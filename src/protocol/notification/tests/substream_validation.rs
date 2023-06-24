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
    error::{Error, SubstreamError},
    mock::substream::MockSubstream,
    peer_id::PeerId,
    protocol::{
        notification::{
            tests::{add_peer, make_notification_protocol},
            types::{NotificationEvent, ValidationResult},
            PeerContext, PeerState,
        },
        ProtocolEvent,
    },
    types::protocol::ProtocolName,
};

use bytes::BytesMut;

use std::task::Poll;

#[tokio::test]
async fn non_existent_peer() {
    let (mut notif, _handle, _sender) = make_notification_protocol();

    if let Err(err) = notif
        .on_validation_result(PeerId::random(), ValidationResult::Accept)
        .await
    {
        assert!(std::matches!(err, Error::PeerDoesntExist(_)));
    }
}

#[tokio::test]
async fn substream_accepted() {
    let (mut notif, mut handle, _sender) = make_notification_protocol();
    let (peer, service, mut receiver) = add_peer();
    let handshake = BytesMut::from(&b"hello"[..]);
    let mut substream = MockSubstream::new();
    substream
        .expect_poll_next()
        .times(1)
        .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
    substream
        .expect_poll_ready()
        .times(1)
        .return_once(|_| Poll::Ready(Ok(())));
    substream
        .expect_start_send()
        .times(1)
        .return_once(|_| Ok(()));
    substream
        .expect_poll_flush()
        .times(1)
        .return_once(|_| Poll::Ready(Ok(())));

    // connect peer and verify it's in closed state
    notif
        .on_connection_established(peer, service)
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::Closed => {}
        _ => panic!("invalid state for peer"),
    }

    // open inbound substream and verify that peer state has changed to `InboundOpen`
    notif
        .on_inbound_substream(ProtocolName::from("/notif/1"), peer, Box::new(substream))
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::InboundOpen { .. } => {}
        _ => panic!("invalid state for peer"),
    }

    // user protocol receives the protocol accepts it
    assert_eq!(
        handle.next_event().await.unwrap(),
        NotificationEvent::ValidateSubstream {
            protocol: ProtocolName::from("/notif/1"),
            peer,
            handshake: handshake.into()
        },
    );
    notif
        .on_validation_result(peer, ValidationResult::Accept)
        .await
        .unwrap();

    // protocol asks for outbound substream to be opened and its state is changed accordingly
    assert_eq!(
        receiver.recv().await.unwrap(),
        ProtocolEvent::OpenSubstream {
            protocol: ProtocolName::from("/notif/1"),
            substream_id: 0usize
        },
    );

    match notif.peers.get(&peer).unwrap().state {
        PeerState::InboundOpenOutboundInitiated { .. } => {}
        _ => panic!("invalid state for peer"),
    }
}

#[tokio::test]
async fn substream_rejected() {
    let (mut notif, mut handle, _sender) = make_notification_protocol();
    let (peer, service, mut receiver) = add_peer();
    let handshake = BytesMut::from(&b"hello"[..]);
    let mut substream = MockSubstream::new();
    substream
        .expect_poll_next()
        .times(1)
        .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
    substream
        .expect_poll_close()
        .times(1)
        .return_once(|_| Poll::Ready(Ok(())));

    // connect peer and verify it's in closed state
    notif
        .on_connection_established(peer, service)
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::Closed => {}
        _ => panic!("invalid state for peer"),
    }

    // open inbound substream and verify that peer state has changed to `InboundOpen`
    notif
        .on_inbound_substream(ProtocolName::from("/notif/1"), peer, Box::new(substream))
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::InboundOpen { .. } => {}
        _ => panic!("invalid state for peer"),
    }

    // user protocol receives the protocol accepts it
    assert_eq!(
        handle.next_event().await.unwrap(),
        NotificationEvent::ValidateSubstream {
            protocol: ProtocolName::from("/notif/1"),
            peer,
            handshake: handshake.into()
        },
    );
    notif
        .on_validation_result(peer, ValidationResult::Reject)
        .await
        .unwrap();

    // substream is rejected so no outbound substraem is opened and peer is converted to closed state
    assert!(receiver.try_recv().is_err());

    match notif.peers.get(&peer).unwrap().state {
        PeerState::Closed => {}
        _ => panic!("invalid state for peer"),
    }
}

#[tokio::test]
async fn accept_fails_due_to_closed_substream() {
    let (mut notif, mut handle, _sender) = make_notification_protocol();
    let (peer, service, mut receiver) = add_peer();
    let handshake = BytesMut::from(&b"hello"[..]);
    let mut substream = MockSubstream::new();
    substream
        .expect_poll_next()
        .times(1)
        .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
    substream
        .expect_poll_ready()
        .times(1)
        .return_once(|_| Poll::Ready(Err(Error::SubstreamError(SubstreamError::ConnectionClosed))));

    // connect peer and verify it's in closed state
    notif
        .on_connection_established(peer, service)
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::Closed => {}
        _ => panic!("invalid state for peer"),
    }

    // open inbound substream and verify that peer state has changed to `InboundOpen`
    notif
        .on_inbound_substream(ProtocolName::from("/notif/1"), peer, Box::new(substream))
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::InboundOpen { .. } => {}
        _ => panic!("invalid state for peer"),
    }

    // user protocol receives the protocol accepts it
    assert_eq!(
        handle.next_event().await.unwrap(),
        NotificationEvent::ValidateSubstream {
            protocol: ProtocolName::from("/notif/1"),
            peer,
            handshake: handshake.into()
        },
    );
    assert!(notif
        .on_validation_result(peer, ValidationResult::Accept)
        .await
        .is_err());

    // substream is not writable so no outbound substream request is made
    // and the connection is marked as closed
    assert!(receiver.try_recv().is_err());

    match notif.peers.get(&peer).unwrap().state {
        PeerState::Closed => {}
        _ => panic!("invalid state for peer"),
    }
}

#[tokio::test]
async fn accept_fails_due_to_closed_connection() {
    let (mut notif, mut handle, _sender) = make_notification_protocol();
    let (peer, service, receiver) = add_peer();
    let handshake = BytesMut::from(&b"hello"[..]);
    let mut substream = MockSubstream::new();
    substream
        .expect_poll_next()
        .times(1)
        .return_once(|_| Poll::Ready(Some(Ok(BytesMut::from(&b"hello"[..])))));
    substream
        .expect_poll_ready()
        .times(1)
        .return_once(|_| Poll::Ready(Ok(())));
    substream
        .expect_start_send()
        .times(1)
        .return_once(|_| Ok(()));
    substream
        .expect_poll_flush()
        .times(1)
        .return_once(|_| Poll::Ready(Ok(())));

    // connect peer and verify it's in closed state
    notif
        .on_connection_established(peer, service)
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::Closed => {}
        _ => panic!("invalid state for peer"),
    }

    // open inbound substream and verify that peer state has changed to `InboundOpen`
    notif
        .on_inbound_substream(ProtocolName::from("/notif/1"), peer, Box::new(substream))
        .await
        .unwrap();

    match notif.peers.get(&peer).unwrap().state {
        PeerState::InboundOpen { .. } => {}
        _ => panic!("invalid state for peer"),
    }

    // user protocol receives the protocol accepts it
    assert_eq!(
        handle.next_event().await.unwrap(),
        NotificationEvent::ValidateSubstream {
            protocol: ProtocolName::from("/notif/1"),
            peer,
            handshake: handshake.into()
        },
    );

    // drop the connection and verify that the protocol doesn't make any outbound substream requests
    // and instead marks the connection as closed
    drop(receiver);

    assert!(notif
        .on_validation_result(peer, ValidationResult::Accept)
        .await
        .is_err());

    match notif.peers.get(&peer).unwrap().state {
        PeerState::Closed => {}
        _ => panic!("invalid state for peer"),
    }
}

#[tokio::test]
#[should_panic]
#[cfg(debug_assertions)]
async fn open_substream_accepted() {
    let (mut notif, _handle, _sender) = make_notification_protocol();
    let (peer, service, _receiver) = add_peer();
    let outbound = Box::new(MockSubstream::new());
    let inbound = Box::new(MockSubstream::new());

    notif.peers.insert(
        peer,
        PeerContext {
            service,
            state: PeerState::Open { outbound },
        },
    );

    notif
        .on_notification_stream_opened(
            peer,
            ProtocolName::from("/notif/1"),
            BytesMut::from(vec![1, 2, 3, 4].as_slice()),
            inbound,
        )
        .await
        .unwrap();

    assert!(notif
        .on_validation_result(peer, ValidationResult::Accept)
        .await
        .is_err());
}

#[tokio::test]
#[should_panic]
#[cfg(debug_assertions)]
async fn open_substream_rejected() {
    let (mut notif, _handle, _sender) = make_notification_protocol();
    let (peer, service, _receiver) = add_peer();
    let outbound = Box::new(MockSubstream::new());
    let inbound = Box::new(MockSubstream::new());

    notif.peers.insert(
        peer,
        PeerContext {
            service,
            state: PeerState::Open { outbound },
        },
    );

    notif
        .on_notification_stream_opened(
            peer,
            ProtocolName::from("/notif/1"),
            BytesMut::from(vec![1, 2, 3, 4].as_slice()),
            inbound,
        )
        .await
        .unwrap();

    assert!(notif
        .on_validation_result(peer, ValidationResult::Reject)
        .await
        .is_err());
}
