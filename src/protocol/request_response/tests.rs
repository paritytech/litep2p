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
    crypto::ed25519::Keypair,
    mock::substream::DummySubstream,
    mock::substream::MockSubstream,
    protocol::{
        connection::ConnectionHandle,
        request_response::{
            ConfigBuilder, DialOptions, RequestResponseError, RequestResponseEvent,
            RequestResponseHandle, RequestResponseProtocol,
        },
        InnerTransportEvent, TransportService,
    },
    substream::Substream,
    transport::{
        dummy::DummyTransport,
        manager::{SupportedTransport, TransportManager},
    },
    types::{ConnectionId, RequestId, SubstreamId},
    BandwidthSink, Endpoint, Error, PeerId, ProtocolName,
};

use futures::StreamExt;
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use tokio::sync::mpsc::{channel, Sender};

use std::{collections::HashSet, net::Ipv4Addr, task::Poll};

// create new protocol for testing
fn protocol() -> (
    RequestResponseProtocol,
    RequestResponseHandle,
    TransportManager,
    Sender<InnerTransportEvent>,
) {
    let (manager, handle) = TransportManager::new(
        Keypair::generate(),
        HashSet::new(),
        BandwidthSink::new(),
        8usize,
    );

    let peer = PeerId::random();
    let (transport_service, tx) = TransportService::new(
        peer,
        ProtocolName::from("/notif/1"),
        Vec::new(),
        std::sync::Arc::new(Default::default()),
        handle,
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
#[should_panic]
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
            Substream::new_mock(peer, Box::new(MockSubstream::new())),
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
        .on_substream_open_failure(SubstreamId::from(1338usize), Error::Unknown)
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
    assert!(protocol.on_cancel_request(request_id).await.is_ok());
}

#[tokio::test]
async fn substream_event_for_unknown_peer() {
    let (mut protocol, _handle, _manager, _tx) = protocol();

    // register peer
    let peer = PeerId::random();
    protocol.on_connection_established(peer).await.unwrap();
    assert!(protocol.peers.contains_key(&peer));

    match protocol
        .on_substream_event(peer, RequestId::from(1337usize), Ok(vec![13, 37]))
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
        .return_once(|_| Poll::Ready(Some(Err(Error::Unknown))));

    // register inbound substream from peer
    protocol
        .on_inbound_substream(peer, None, Substream::new_mock(peer, Box::new(substream)))
        .await
        .unwrap();

    // verify the request has been registered for the peer
    let request_id = *protocol.peers.get(&peer).unwrap().active_inbound.keys().next().unwrap();
    assert!(protocol.pending_inbound_requests.get_mut(&(peer, request_id)).is_some());

    // poll the substream and get the failure event
    let ((peer, request_id), event) = protocol.pending_inbound_requests.next().await.unwrap();

    match protocol.on_inbound_request(peer, request_id, event).await {
        Err(Error::InvalidData) => {}
        _ => panic!("invalid return value"),
    }
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
            Substream::new_mock(peer, Box::new(DummySubstream::new())),
        )
        .await
        .unwrap();

    // verify the request has been registered for the peer
    let request_id = *protocol.peers.get(&peer).unwrap().active_inbound.keys().next().unwrap();
    assert!(protocol.pending_inbound_requests.get_mut(&(peer, request_id)).is_some());

    // disconnect the peer and verify that no events are read from the handle
    // since no outbound request was initiated
    protocol.on_connection_closed(peer).await;

    futures::future::poll_fn(|cx| match handle.poll_next_unpin(cx) {
        Poll::Pending => Poll::Ready(()),
        event => panic!("read an unexpected event from handle: {event:?}"),
    })
    .await;

    // verify the substream has been removed from `pending_inbound_requests`
    assert!(protocol.pending_inbound_requests.get_mut(&(peer, request_id)).is_none());
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
    protocol
        .on_send_request(
            peer,
            RequestId::from(1337usize),
            vec![1, 2, 3, 4],
            DialOptions::Reject,
        )
        .await
        .unwrap();

    match handle.next().await {
        Some(RequestResponseEvent::RequestFailed {
            peer: request_peer,
            request_id,
            error,
        }) => {
            assert_eq!(request_peer, peer);
            assert_eq!(request_id, RequestId::from(1337usize));
            assert_eq!(error, RequestResponseError::Rejected);
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

// depending on what kind of executor is used, and more precisely, how the tasks of litep2p get
// executed, what can potentially happen is that connection is established to a peer that was dialed
// and the request-response protocol got polled with such delay that the connection's keep-alive
// timeout had expired before it was registered to the protocol and it was closed.
//
// in this situation, a stale `ConnectionEstablished` is read from the `TransportService` and when
// the protocol attempts to open a substream, the call will fail because the connection is no longer
// open. Under normal circumstances, the request failure has to be reported gracefully, but it will
// cause a `debug_assert!()` as it's a sign there might be something worth investigating
#[tokio::test]
#[cfg(debug_assertions)]
#[should_panic]
async fn stale_connection() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut protocol, _handle, mut manager, tx) = protocol();

    // register `DummyTransport` as TCP to `TransportManager` and then register a known peer
    // register peer to `TransportManager`
    let peer = PeerId::random();
    manager.register_transport(SupportedTransport::Tcp, Box::new(DummyTransport::new()));
    manager.add_known_address(
        peer,
        vec![Multiaddr::empty()
            .with(Protocol::Ip4(Ipv4Addr::new(127, 0, 0, 1)))
            .with(Protocol::Tcp(8888))
            .with(Protocol::P2p(Multihash::from(peer)))]
        .into_iter(),
    );

    // initiate outbound request and instruct the protocol to dial the peer
    protocol
        .on_send_request(
            peer,
            RequestId::from(1337usize),
            vec![1, 2, 3, 4],
            DialOptions::Dial,
        )
        .await
        .unwrap();

    // create new established connection
    let (conn_tx, conn_rx) = channel(1);
    let sender = ConnectionHandle::new(ConnectionId::from(1337usize), conn_tx);

    tx.send(InnerTransportEvent::ConnectionEstablished {
        peer,
        connection: ConnectionId::from(1337usize),
        endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1337usize)),
        sender,
    })
    .await
    .unwrap();

    // drop `conn_rx` to signal that the connection was closed
    drop(conn_rx);
    protocol.run().await;
}
