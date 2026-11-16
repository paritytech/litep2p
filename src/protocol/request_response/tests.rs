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
        request_response::{
            ConfigBuilder, DialOptions, RejectReason, RequestContext, RequestResponseError,
            RequestResponseEvent, RequestResponseHandle, RequestResponseProtocol,
        },
        InnerTransportEvent, ProtocolCommand, SubstreamError, SubstreamKeepAlive, TransportService,
    },
    substream::Substream,
    transport::{
        manager::{TransportManager, TransportManagerBuilder},
        Endpoint, KEEP_ALIVE_TIMEOUT,
    },
    types::{ConnectionId, RequestId, SubstreamId},
    Error, PeerId, ProtocolName,
};

use bytes::BytesMut;
use futures::{FutureExt, StreamExt};
use multiaddr::Multiaddr;
use tokio::sync::mpsc::{channel, Sender};

use std::{io::ErrorKind, task::Poll, time::Duration};

// create new protocol for testing
fn protocol() -> (
    RequestResponseProtocol,
    RequestResponseHandle,
    TransportManager,
    Sender<InnerTransportEvent>,
) {
    let manager = TransportManagerBuilder::new().build();

    let peer = PeerId::random();
    let (transport_service, tx) = TransportService::new(
        peer,
        ProtocolName::from("/notif/1"),
        Vec::new(),
        std::sync::Arc::new(Default::default()),
        manager.transport_manager_handle(),
        KEEP_ALIVE_TIMEOUT,
        SubstreamKeepAlive::Yes,
    );
    let (config, handle) =
        ConfigBuilder::new(ProtocolName::from("/req/1")).with_max_size(1024).build();

    (
        RequestResponseProtocol::new(transport_service, config),
        handle,
        manager,
        tx,
    )
}

#[tokio::test]
#[cfg(debug_assertions)]
#[should_panic]
async fn connection_closed_twice() {
    let (mut protocol, _handle, _manager, _tx) = protocol();

    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();
    assert!(protocol.peers.contains_key(&peer));

    protocol.on_connection_established(peer).await.unwrap();
}

#[tokio::test]
#[cfg(debug_assertions)]
async fn connection_established_twice() {
    let (mut protocol, _handle, _manager, _tx) = protocol();

    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();
    assert!(protocol.peers.contains_key(&peer));

    protocol.on_connection_closed(peer).await;
    assert!(!protocol.peers.contains_key(&peer));

    protocol.on_connection_closed(peer).await;
}

#[tokio::test]
#[cfg(debug_assertions)]
#[should_panic]
async fn unknown_outbound_substream_opened() {
    let (mut protocol, _handle, _manager, _tx) = protocol();
    let peer = PeerId::random();

    match protocol
        .on_outbound_substream(
            peer,
            SubstreamId::from(1337usize),
            Substream::new_mock(
                peer,
                SubstreamId::from(0usize),
                Box::new(MockSubstream::new()),
            ),
            None,
        )
        .await
    {
        Err(Error::InvalidState) => {}
        _ => panic!("invalid return value"),
    }
}

#[tokio::test]
#[cfg(debug_assertions)]
#[should_panic]
async fn unknown_substream_open_failure() {
    let (mut protocol, _handle, _manager, _tx) = protocol();

    match protocol
        .on_substream_open_failure(
            SubstreamId::from(1338usize),
            SubstreamError::ConnectionClosed,
        )
        .await
    {
        Err(Error::InvalidState) => {}
        _ => panic!("invalid return value"),
    }
}

#[tokio::test]
async fn cancel_unknown_request() {
    let (mut protocol, _handle, _manager, _tx) = protocol();

    let request_id = RequestId::from(1337usize);
    assert!(!protocol.pending_outbound_cancels.contains_key(&request_id));
    assert!(protocol.on_cancel_request(request_id).is_ok());
}

#[tokio::test]
async fn substream_event_for_unknown_peer() {
    let (mut protocol, _handle, _manager, _tx) = protocol();

    // register peer
    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();
    assert!(protocol.peers.contains_key(&peer));

    match protocol
        .on_substream_event(peer, RequestId::from(1337usize), None, Ok(vec![13, 37]))
        .await
    {
        Err(Error::InvalidState) => {}
        _ => panic!("invalid return value"),
    }
}

#[tokio::test]
async fn inbound_substream_error() {
    let (mut protocol, _handle, _manager, _tx) = protocol();

    // register peer
    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();
    assert!(protocol.peers.contains_key(&peer));

    let mut substream = MockSubstream::new();
    substream
        .expect_poll_next()
        .times(1)
        .return_once(|_| Poll::Ready(Some(Err(SubstreamError::ConnectionClosed))));

    // register inbound substream from peer
    protocol
        .on_inbound_substream(
            peer,
            None,
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        )
        .await
        .unwrap();

    // poll the substream and get the failure event
    assert_eq!(protocol.pending_inbound_requests.len(), 1);
    let (peer, request_id, event, substream) =
        protocol.pending_inbound_requests.next().await.unwrap();

    match protocol.on_inbound_request(peer, request_id, event, substream).await {
        Err(Error::InvalidData) => {}
        _ => panic!("invalid return value"),
    }
}

#[tokio::test]
async fn inbound_request_read_times_out() {
    let (mut protocol, _handle, _manager, _tx) = protocol();
    protocol.timeout = Duration::from_millis(100);

    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();
    protocol
        .on_inbound_substream(
            peer,
            None,
            Substream::new_mock(
                peer,
                SubstreamId::from(0usize),
                Box::new(DummySubstream::new()),
            ),
        )
        .await
        .unwrap();

    let (peer, request_id, request, substream) =
        protocol.pending_inbound_requests.next().await.unwrap();
    assert_eq!(request, Err(SubstreamError::IoError(ErrorKind::TimedOut)));
    assert!(protocol.peers.get(&peer).unwrap().active_inbound.contains_key(&request_id));

    assert!(matches!(
        protocol.on_inbound_request(peer, request_id, request, substream).await,
        Err(Error::InvalidData)
    ));
    assert!(protocol.peers.get(&peer).unwrap().active_inbound.is_empty());
    assert!(protocol.pending_inbound_requests.is_empty());
    assert!(protocol.pending_outbound_responses.is_empty());
}

#[tokio::test]
async fn rejected_inbound_substream_does_not_wait_for_close() {
    let (mut protocol, _handle, _manager, _tx) = protocol();
    protocol.max_concurrent_inbound_requests = Some(0);

    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();

    let mut substream = MockSubstream::new();
    substream.expect_poll_close().returning(|_| Poll::Pending);

    let result = protocol
        .on_inbound_substream(
            peer,
            None,
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        )
        .now_or_never();

    assert!(matches!(result, Some(Ok(()))));
    assert_eq!(protocol.pending_rejected_closes.len(), 1);
    assert!(protocol.pending_inbound_requests.is_empty());
}

#[tokio::test]
async fn closing_rejected_substreams_does_not_occupy_request_slots() {
    let (mut protocol, _handle, _manager, _tx) = protocol();
    protocol.max_concurrent_inbound_requests = Some(1);

    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();

    let mut active = MockSubstream::new();
    active.expect_poll_next().returning(|_| Poll::Ready(None));
    protocol
        .on_inbound_substream(
            peer,
            None,
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(active)),
        )
        .await
        .unwrap();

    // A close that never completes, like a QUIC finish waiting for the peer's ACK.
    let mut rejected = MockSubstream::new();
    rejected.expect_poll_close().returning(|_| Poll::Pending);
    protocol
        .on_inbound_substream(
            peer,
            None,
            Substream::new_mock(peer, SubstreamId::from(1usize), Box::new(rejected)),
        )
        .await
        .unwrap();
    assert!(protocol.pending_rejected_closes.next().now_or_never().is_none());

    let (peer, request_id, request, substream) =
        protocol.pending_inbound_requests.next().await.unwrap();
    let _ = protocol.on_inbound_request(peer, request_id, request, substream).await;
    assert!(protocol.pending_inbound_requests.is_empty());

    protocol
        .on_inbound_substream(
            peer,
            None,
            Substream::new_mock(
                peer,
                SubstreamId::from(2usize),
                Box::new(DummySubstream::new()),
            ),
        )
        .await
        .unwrap();
    assert_eq!(protocol.pending_inbound_requests.len(), 1);
}

#[tokio::test]
async fn outbound_request_timeout_does_not_wait_for_close() {
    let (mut protocol, _handle, _manager, _tx) = protocol();
    protocol.timeout = Duration::from_millis(100);

    let peer = PeerId::random();
    let substream_id = SubstreamId::from(0usize);
    let request_id = RequestId::from(0usize);
    protocol.pending_outbound.insert(
        substream_id,
        RequestContext::new(peer, request_id, vec![1, 2, 3], None),
    );

    let mut substream = MockSubstream::new();
    substream.expect_poll_ready().returning(|_| Poll::Ready(Ok(())));
    substream.expect_start_send().returning(|_| Ok(()));
    substream.expect_poll_flush().returning(|_| Poll::Ready(Ok(())));
    substream.expect_poll_next().returning(|_| Poll::Pending);
    substream.expect_poll_close().returning(|_| Poll::Pending);

    protocol
        .on_outbound_substream(
            peer,
            substream_id,
            Substream::new_mock(peer, substream_id, Box::new(substream)),
            None,
        )
        .await
        .unwrap();

    let (_, _, _, result) =
        tokio::time::timeout(Duration::from_secs(5), protocol.pending_inbound.next())
            .await
            .expect("timeout must not wait for the substream to close")
            .unwrap();
    assert!(matches!(result, Err(RequestResponseError::Timeout)));
}

#[tokio::test]
async fn rejected_inbound_request_does_not_wait_for_close() {
    let (mut protocol, mut handle, _manager, _tx) = protocol();

    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();
    let request_id = protocol.next_request_id();
    protocol.peers.get_mut(&peer).unwrap().active_inbound.insert(request_id, None);

    let mut substream = MockSubstream::new();
    substream.expect_poll_close().returning(|_| Poll::Pending);

    protocol
        .on_inbound_request(
            peer,
            request_id,
            Ok(BytesMut::from(&b"hello"[..])),
            Substream::new_mock(peer, SubstreamId::from(0usize), Box::new(substream)),
        )
        .await
        .unwrap();
    assert!(matches!(
        handle.next().await,
        Some(RequestResponseEvent::RequestReceived { .. })
    ));
    handle.reject_request(request_id);

    tokio::time::timeout(
        Duration::from_secs(1),
        protocol.pending_outbound_responses.next(),
    )
    .await
    .expect("rejection must not wait for the substream to close");
    assert!(protocol.pending_outbound_responses.is_empty());
}

// when a peer who had an active inbound substream disconnects, verify that the substream is removed
// from `pending_inbound_requests` so it doesn't generate new wake-up notifications
#[tokio::test]
async fn disconnect_peer_has_active_inbound_substream() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut protocol, mut handle, _manager, _tx) = protocol();

    // register new peer
    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();

    // register inbound substream from peer
    protocol
        .on_inbound_substream(
            peer,
            None,
            Substream::new_mock(
                peer,
                SubstreamId::from(0usize),
                Box::new(DummySubstream::new()),
            ),
        )
        .await
        .unwrap();

    assert_eq!(protocol.pending_inbound_requests.len(), 1);

    // disconnect the peer and verify that no events are read from the handle
    // since no outbound request was initiated
    protocol.on_connection_closed(peer).await;

    futures::future::poll_fn(|cx| match handle.poll_next_unpin(cx) {
        Poll::Pending => Poll::Ready(()),
        event => panic!("read an unexpected event from handle: {event:?}"),
    })
    .await;
}

// when user initiates an outbound request and `RequestResponseProtocol` tries to open an outbound
// substream to them and it fails, the failure should be reported to the user. When the remote peer
// later disconnects, this failure should not be reported again.
#[tokio::test]
async fn request_failure_reported_once() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut protocol, mut handle, _manager, _tx) = protocol();

    // register new peer
    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();

    // initiate outbound request
    //
    // since the peer wasn't properly registered, opening substream to them will fail
    let request_id = RequestId::from(1337usize);
    let error = protocol
        .on_send_request(
            peer,
            request_id,
            vec![1, 2, 3, 4],
            DialOptions::Reject,
            None,
        )
        .unwrap_err();
    protocol.report_request_failure(peer, request_id, error).await.unwrap();

    match handle.next().await {
        Some(RequestResponseEvent::RequestFailed {
            peer: request_peer,
            request_id,
            error,
        }) => {
            assert_eq!(request_peer, peer);
            assert_eq!(request_id, RequestId::from(1337usize));
            assert!(matches!(error, RequestResponseError::Rejected(_)));
        }
        event => panic!("unexpected event: {event:?}"),
    }

    // disconnect the peer and verify that no events are read from the handle
    // since the outbound request failure was already reported
    protocol.on_connection_closed(peer).await;

    futures::future::poll_fn(|cx| match handle.poll_next_unpin(cx) {
        Poll::Pending => Poll::Ready(()),
        event => panic!("read an unexpected event from handle: {event:?}"),
    })
    .await;
}

// when the primary connection is closed while a secondary connection exists, the protocol is not
// notified about the closed connection, but a request that was already issued over it can never
// succeed and must be reported as failed
#[tokio::test]
async fn request_over_replaced_primary_connection_fails() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (protocol, mut handle, _manager, tx) = protocol();
    let peer = PeerId::random();

    // register primary and secondary connection before the event loop is started so that they
    // are handled before the request, `RequestResponseProtocol` polls network events first
    let (cmd_tx1, mut cmd_rx1) = channel(64);
    tx.send(InnerTransportEvent::ConnectionEstablished {
        peer,
        connection: ConnectionId::from(0usize),
        endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(0usize)),
        sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
    })
    .await
    .unwrap();

    let (cmd_tx2, _cmd_rx2) = channel(64);
    tx.send(InnerTransportEvent::ConnectionEstablished {
        peer,
        connection: ConnectionId::from(1usize),
        endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1usize)),
        sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
    })
    .await
    .unwrap();

    tokio::spawn(protocol.run());

    let request_id =
        handle.send_request(peer, vec![1, 3, 3, 7], DialOptions::Reject).await.unwrap();

    // the request is issued over the primary connection and the transport never reports back
    match cmd_rx1.recv().await {
        Some(ProtocolCommand::OpenSubstream { connection_id, .. }) => {
            assert_eq!(connection_id, ConnectionId::from(0usize));
        }
        command => panic!("unexpected command: {command:?}"),
    }

    tx.send(InnerTransportEvent::ConnectionClosed {
        peer,
        connection: ConnectionId::from(0usize),
    })
    .await
    .unwrap();

    match handle.next().await {
        Some(RequestResponseEvent::RequestFailed {
            peer: failed_peer,
            request_id: failed_request,
            error,
        }) => {
            assert_eq!(failed_peer, peer);
            assert_eq!(failed_request, request_id);
            assert!(std::matches!(
                error,
                RequestResponseError::Rejected(RejectReason::ConnectionClosed)
            ));
        }
        event => panic!("unexpected event: {event:?}"),
    }
}
