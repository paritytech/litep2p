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
    protocol::{
        request_response::{
            ConfigBuilder, DialOptions, RequestResponseError, RequestResponseEvent,
            RequestResponseHandle, RequestResponseProtocol,
        },
        InnerTransportEvent, TransportService,
    },
    substream::Substream,
    transport::manager::TransportManager,
    types::RequestId,
    BandwidthSink, PeerId, ProtocolName,
};

use futures::StreamExt;
use tokio::sync::mpsc::Sender;

use std::{collections::HashSet, task::Poll};

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
