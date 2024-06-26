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
    error::Error,
    protocol::{connection::ConnectionHandle, InnerTransportEvent, TransportEvent},
    transport::{manager::TransportManagerHandle, Endpoint},
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId, DEFAULT_CHANNEL_SIZE,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, Stream, StreamExt};
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use tokio::sync::mpsc::{channel, Receiver, Sender};

use std::{
    collections::{HashMap, HashSet},
    fmt::Debug,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::transport-service";

/// Connection context for the peer.
///
/// Each peer is allowed to have at most two connections open. The first open connection is the
/// primary connections which the local node uses to open substreams to remote. Secondary connection
/// may be open if local and remote opened connections at the same time.
///
/// Secondary connection may be promoted to a primary connection if the primary connections closes
/// while the secondary connections remains open.
#[derive(Debug)]
struct ConnectionContext {
    /// Primary connection.
    primary: ConnectionHandle,

    /// Secondary connection, if it exists.
    secondary: Option<ConnectionHandle>,
}

impl ConnectionContext {
    /// Create new [`ConnectionContext`].
    fn new(primary: ConnectionHandle) -> Self {
        Self {
            primary,
            secondary: None,
        }
    }

    /// Downgrade connection to non-active which means it will be closed
    /// if there are no substreams open over it.
    fn downgrade(&mut self, connection_id: &ConnectionId) {
        if self.primary.connection_id() == connection_id {
            self.primary.close();
            return;
        }

        if let Some(handle) = &mut self.secondary {
            if handle.connection_id() == connection_id {
                handle.close();
                return;
            }
        }

        tracing::debug!(
            target: LOG_TARGET,
            primary = ?self.primary.connection_id(),
            secondary = ?self.secondary.as_ref().map(|handle| handle.connection_id()),
            ?connection_id,
            "connection doesn't exist, cannot downgrade",
        );
    }
}

/// Provides an interfaces for [`Litep2p`](crate::Litep2p) protocols to interact
/// with the underlying transport protocols.
#[derive(Debug)]
pub struct TransportService {
    /// Local peer ID.
    pub(crate) local_peer_id: PeerId,

    /// Protocol.
    protocol: ProtocolName,

    /// Fallback names for the protocol.
    fallback_names: Vec<ProtocolName>,

    /// Open connections.
    connections: HashMap<PeerId, ConnectionContext>,

    /// Transport handle.
    transport_handle: TransportManagerHandle,

    /// RX channel for receiving events from tranports and connections.
    rx: Receiver<InnerTransportEvent>,

    /// Next substream ID.
    next_substream_id: Arc<AtomicUsize>,

    /// Close the connection if no substreams are open within this time frame.
    keep_alive: Duration,

    /// Pending keep-alive timeouts.
    keep_alive_timeouts: FuturesUnordered<BoxFuture<'static, (PeerId, ConnectionId)>>,
}

impl TransportService {
    /// Create new [`TransportService`].
    pub(crate) fn new(
        local_peer_id: PeerId,
        protocol: ProtocolName,
        fallback_names: Vec<ProtocolName>,
        next_substream_id: Arc<AtomicUsize>,
        transport_handle: TransportManagerHandle,
        keep_alive: Duration,
    ) -> (Self, Sender<InnerTransportEvent>) {
        let (tx, rx) = channel(DEFAULT_CHANNEL_SIZE);

        (
            Self {
                rx,
                protocol,
                local_peer_id,
                fallback_names,
                transport_handle,
                next_substream_id,
                connections: HashMap::new(),
                keep_alive,
                keep_alive_timeouts: FuturesUnordered::new(),
            },
            tx,
        )
    }

    /// Handle connection established event.
    fn on_connection_established(
        &mut self,
        peer: PeerId,
        endpoint: Endpoint,
        connection_id: ConnectionId,
        handle: ConnectionHandle,
    ) -> Option<TransportEvent> {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            protocol = %self.protocol,
            ?endpoint,
            ?connection_id,
            "connection established",
        );
        let keep_alive = self.keep_alive;

        match self.connections.get_mut(&peer) {
            Some(context) => match context.secondary {
                Some(_) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?connection_id,
                        ?endpoint,
                        "ignoring third connection",
                    );
                    None
                }
                None => {
                    self.keep_alive_timeouts.push(Box::pin(async move {
                        tokio::time::sleep(keep_alive).await;
                        (peer, connection_id)
                    }));
                    context.secondary = Some(handle);

                    None
                }
            },
            None => {
                self.connections.insert(peer, ConnectionContext::new(handle));
                self.keep_alive_timeouts.push(Box::pin(async move {
                    tokio::time::sleep(keep_alive).await;
                    (peer, connection_id)
                }));

                Some(TransportEvent::ConnectionEstablished { peer, endpoint })
            }
        }
    }

    /// Handle connection closed event.
    fn on_connection_closed(
        &mut self,
        peer: PeerId,
        connection_id: ConnectionId,
    ) -> Option<TransportEvent> {
        let Some(context) = self.connections.get_mut(&peer) else {
            tracing::warn!(
                target: LOG_TARGET,
                ?peer,
                ?connection_id,
                "connection closed to a non-existent peer",
            );

            debug_assert!(false);
            return None;
        };

        // if the primary connection was closed, check if there exist a secondary connection
        // and if it does, convert the secondary connection a primary connection
        if context.primary.connection_id() == &connection_id {
            tracing::trace!(target: LOG_TARGET, ?peer, ?connection_id, "primary connection closed");

            match context.secondary.take() {
                None => {
                    self.connections.remove(&peer);
                    return Some(TransportEvent::ConnectionClosed { peer });
                }
                Some(handle) => {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?peer,
                        ?connection_id,
                        "switch to secondary connection",
                    );

                    context.primary = handle;
                    return None;
                }
            }
        }

        match context.secondary.take() {
            Some(handle) if handle.connection_id() == &connection_id => {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    "secondary connection closed",
                );

                None
            }
            connection_state => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    ?connection_state,
                    "connection closed but it doesn't exist",
                );

                None
            }
        }
    }

    /// Dial `peer` using `PeerId`.
    ///
    /// Call fails if `Litep2p` doesn't have a known address for the peer.
    pub fn dial(&mut self, peer: &PeerId) -> crate::Result<()> {
        self.transport_handle.dial(peer)
    }

    /// Dial peer using a `Multiaddr`.
    ///
    /// Call fails if the address is not in correct format or it contains an unsupported/disabled
    /// transport.
    ///
    /// Calling this function is only necessary for those addresses that are discovered out-of-band
    /// since `Litep2p` internally keeps track of all peer addresses it has learned through user
    /// calling this function, Kademlia peer discoveries and `Identify` responses.
    pub fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()> {
        self.transport_handle.dial_address(address)
    }

    /// Add one or more addresses for `peer`.
    ///
    /// The list is filtered for duplicates and unsupported transports.
    pub fn add_known_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>) {
        let addresses: HashSet<Multiaddr> = addresses
            .filter_map(|address| {
                if !std::matches!(address.iter().last(), Some(Protocol::P2p(_))) {
                    Some(address.with(Protocol::P2p(Multihash::from_bytes(&peer.to_bytes()).ok()?)))
                } else {
                    Some(address)
                }
            })
            .collect();

        self.transport_handle.add_known_address(peer, addresses.into_iter());
    }

    /// Open substream to `peer`.
    ///
    /// Call fails if there is no connection open to `peer` or the channel towards
    /// the connection is clogged.
    pub fn open_substream(&mut self, peer: PeerId) -> crate::Result<SubstreamId> {
        // always prefer the primary connection
        let connection =
            &mut self.connections.get_mut(&peer).ok_or(Error::PeerDoesntExist(peer))?.primary;

        let permit = connection.try_get_permit().ok_or(Error::ConnectionClosed)?;
        let substream_id =
            SubstreamId::from(self.next_substream_id.fetch_add(1usize, Ordering::Relaxed));

        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            protocol = %self.protocol,
            ?substream_id,
            "open substream",
        );

        connection
            .open_substream(
                self.protocol.clone(),
                self.fallback_names.clone(),
                substream_id,
                permit,
            )
            .map(|_| substream_id)
    }

    /// Forcibly close the connection, even if other protocols have substreams open over it.
    pub fn force_close(&mut self, peer: PeerId) -> crate::Result<()> {
        let connection =
            &mut self.connections.get_mut(&peer).ok_or(Error::PeerDoesntExist(peer))?;

        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            protocol = %self.protocol,
            secondary = ?connection.secondary,
            "forcibly closing the connection",
        );

        if let Some(ref mut connection) = connection.secondary {
            let _ = connection.force_close();
        }

        connection.primary.force_close()
    }
}

impl Stream for TransportService {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        while let Poll::Ready(event) = self.rx.poll_recv(cx) {
            match event {
                None => return Poll::Ready(None),
                Some(InnerTransportEvent::ConnectionEstablished {
                    peer,
                    endpoint,
                    sender,
                    connection,
                }) => {
                    if let Some(event) =
                        self.on_connection_established(peer, endpoint, connection, sender)
                    {
                        return Poll::Ready(Some(event));
                    }
                }
                Some(InnerTransportEvent::ConnectionClosed { peer, connection }) => {
                    if let Some(event) = self.on_connection_closed(peer, connection) {
                        return Poll::Ready(Some(event));
                    }
                }
                Some(event) => return Poll::Ready(Some(event.into())),
            }
        }

        while let Poll::Ready(Some((peer, connection_id))) =
            self.keep_alive_timeouts.poll_next_unpin(cx)
        {
            if let Some(context) = self.connections.get_mut(&peer) {
                tracing::trace!(
                    target: LOG_TARGET,
                    ?peer,
                    ?connection_id,
                    "keep-alive timeout over, downgrade connection",
                );

                context.downgrade(&connection_id);
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        protocol::TransportService,
        transport::manager::{handle::InnerTransportManagerCommand, TransportManagerHandle},
    };
    use futures::StreamExt;
    use parking_lot::RwLock;
    use std::collections::HashSet;

    /// Create new `TransportService`
    fn transport_service() -> (
        TransportService,
        Sender<InnerTransportEvent>,
        Receiver<InnerTransportManagerCommand>,
    ) {
        let (cmd_tx, cmd_rx) = channel(64);
        let peer = PeerId::random();

        let handle = TransportManagerHandle::new(
            peer,
            Arc::new(RwLock::new(HashMap::new())),
            cmd_tx,
            HashSet::new(),
            Default::default(),
        );

        let (service, sender) = TransportService::new(
            peer,
            ProtocolName::from("/notif/1"),
            Vec::new(),
            Arc::new(AtomicUsize::new(0usize)),
            handle,
            std::time::Duration::from_secs(5),
        );

        (service, sender, cmd_rx)
    }

    #[tokio::test]
    async fn secondary_connection_stored() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, _cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.poll_next_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );
    }

    #[tokio::test]
    async fn tertiary_connection_ignored() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, _cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.poll_next_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );

        // try to register tertiary connection and verify it's ignored
        let (cmd_tx3, mut cmd_rx3) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(2usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(2usize)),
                sender: ConnectionHandle::new(ConnectionId::from(2usize), cmd_tx3),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.poll_next_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );
        assert!(cmd_rx3.try_recv().is_err());
    }

    #[tokio::test]
    async fn secondary_closing_doesnt_emit_event() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, _cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.poll_next_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );

        // close the secondary connection
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(1usize),
            })
            .await
            .unwrap();

        // verify that the protocol is not notified
        futures::future::poll_fn(|cx| match service.poll_next_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        // verify that the secondary connection doesn't exist anymore
        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert!(context.secondary.is_none());
    }

    #[tokio::test]
    async fn convert_secondary_to_primary() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, mut cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(0usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(0usize)),
                sender: ConnectionHandle::new(ConnectionId::from(0usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // register secondary connection
        let (cmd_tx2, mut cmd_rx2) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(1usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1usize), cmd_tx2),
            })
            .await
            .unwrap();

        futures::future::poll_fn(|cx| match service.poll_next_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(0usize));
        assert_eq!(
            context.secondary.as_ref().unwrap().connection_id(),
            &ConnectionId::from(1usize)
        );

        // close the primary connection
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(0usize),
            })
            .await
            .unwrap();

        // verify that the protocol is not notified
        futures::future::poll_fn(|cx| match service.poll_next_unpin(cx) {
            std::task::Poll::Ready(_) => panic!("didn't expect event from `TransportService`"),
            std::task::Poll::Pending => std::task::Poll::Ready(()),
        })
        .await;

        // verify that the primary connection has been replaced
        let context = service.connections.get(&peer).unwrap();
        assert_eq!(context.primary.connection_id(), &ConnectionId::from(1usize));
        assert!(context.secondary.is_none());
        assert!(cmd_rx1.try_recv().is_err());

        // close the secondary connection as well
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(1usize),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionClosed {
            peer: disconnected_peer,
        }) = service.next().await
        {
            assert_eq!(disconnected_peer, peer);
        } else {
            panic!("expected event from `TransportService`");
        };

        // verify that the primary connection has been replaced
        assert!(service.connections.get(&peer).is_none());
        assert!(cmd_rx2.try_recv().is_err());
    }

    #[tokio::test]
    async fn keep_alive_timeout_expires_for_a_stale_connection() {
        let (mut service, sender, _) = transport_service();
        let peer = PeerId::random();

        // register first connection
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1337usize),
                endpoint: Endpoint::dialer(Multiaddr::empty(), ConnectionId::from(1337usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1337usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // verify the first connection state is correct
        assert_eq!(service.keep_alive_timeouts.len(), 1);
        match service.connections.get(&peer) {
            Some(context) => {
                assert_eq!(
                    context.primary.connection_id(),
                    &ConnectionId::from(1337usize)
                );
                assert!(context.secondary.is_none());
            }
            None => panic!("expected {peer} to exist"),
        }

        // close the primary connection
        sender
            .send(InnerTransportEvent::ConnectionClosed {
                peer,
                connection: ConnectionId::from(1337usize),
            })
            .await
            .unwrap();

        // verify that the protocols are notified of the connection closing as well
        if let Some(TransportEvent::ConnectionClosed {
            peer: connected_peer,
        }) = service.next().await
        {
            assert_eq!(connected_peer, peer);
        } else {
            panic!("expected event from `TransportService`");
        }

        // verify that the keep-alive timeout still exists for the peer but the peer itself
        // doesn't exist anymore
        //
        // the peer is removed because there is no connection to them
        assert_eq!(service.keep_alive_timeouts.len(), 1);
        assert!(service.connections.get(&peer).is_none());

        // register new primary connection but verify that there are now two pending keep-alive
        // timeouts
        let (cmd_tx1, _cmd_rx1) = channel(64);
        sender
            .send(InnerTransportEvent::ConnectionEstablished {
                peer,
                connection: ConnectionId::from(1338usize),
                endpoint: Endpoint::listener(Multiaddr::empty(), ConnectionId::from(1338usize)),
                sender: ConnectionHandle::new(ConnectionId::from(1338usize), cmd_tx1),
            })
            .await
            .unwrap();

        if let Some(TransportEvent::ConnectionEstablished {
            peer: connected_peer,
            endpoint,
        }) = service.next().await
        {
            assert_eq!(connected_peer, peer);
            assert_eq!(endpoint.address(), &Multiaddr::empty());
        } else {
            panic!("expected event from `TransportService`");
        };

        // verify the first connection state is correct
        assert_eq!(service.keep_alive_timeouts.len(), 2);
        match service.connections.get(&peer) {
            Some(context) => {
                assert_eq!(
                    context.primary.connection_id(),
                    &ConnectionId::from(1338usize)
                );
                assert!(context.secondary.is_none());
            }
            None => panic!("expected {peer} to exist"),
        }

        match tokio::time::timeout(Duration::from_secs(10), service.next()).await {
            Ok(event) => panic!("didn't expect an event: {event:?}"),
            Err(_) => {}
        }
    }
}
