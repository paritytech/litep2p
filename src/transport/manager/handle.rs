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
    error::{AddressError, Error},
    executor::Executor,
    protocol::{InnerTransportEvent, ProtocolSet},
    transport::{
        manager::{
            address::{AddressRecord, AddressStore},
            types::{PeerContext, PeerState, SupportedTransport},
            ProtocolContext, TransportManagerCommand, TransportManagerEvent, LOG_TARGET,
        },
        Endpoint,
    },
    types::{protocol::ProtocolName, ConnectionId},
    BandwidthSink, PeerId,
};

use futures::Stream;
use multiaddr::{Multiaddr, Protocol};
use parking_lot::RwLock;
use tokio::sync::mpsc::{Receiver, Sender};

use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
};

/// Inner commands sent from [`TransportManagerHandle`] to [`TransportManager`].
#[derive(Debug)]
pub enum InnerTransportManagerCommand {
    /// Dial peer.
    DialPeer {
        /// Remote peer ID.
        peer: PeerId,
    },

    /// Dial address.
    DialAddress {
        /// Remote address.
        address: Multiaddr,
    },
}

/// Handle for communicating with [`TransportManager`].
#[derive(Debug, Clone)]
pub struct TransportManagerHandle {
    /// Peers.
    peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,

    /// TX channel for sending commands to [`TransportManager`].
    cmd_tx: Sender<InnerTransportManagerCommand>,

    /// Supported transports.
    supported_transport: HashSet<SupportedTransport>,
}

impl TransportManagerHandle {
    /// Create new [`TransportManagerHandle`].
    pub fn new(
        peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,
        cmd_tx: Sender<InnerTransportManagerCommand>,
        supported_transport: HashSet<SupportedTransport>,
    ) -> Self {
        Self {
            peers,
            cmd_tx,
            supported_transport,
        }
    }

    /// Check if `address` is supported by one of the enabled transports.
    pub fn supported_transport(&self, address: &Multiaddr) -> bool {
        let mut iter = address.iter();

        if !std::matches!(
            iter.next(),
            Some(
                Protocol::Ip4(_)
                    | Protocol::Ip6(_)
                    | Protocol::Dns(_)
                    | Protocol::Dns4(_)
                    | Protocol::Dns6(_)
            )
        ) {
            return false;
        }

        match iter.next() {
            None => return false,
            Some(Protocol::Tcp(_)) => match (
                iter.next(),
                self.supported_transport.contains(&SupportedTransport::WebSocket),
            ) {
                (Some(Protocol::Ws(_)), true) => true,
                (Some(Protocol::Wss(_)), true) => true,
                (Some(Protocol::P2p(_)), _) =>
                    self.supported_transport.contains(&SupportedTransport::Tcp),
                _ => return false,
            },
            Some(Protocol::Udp(_)) => match (
                iter.next(),
                self.supported_transport.contains(&SupportedTransport::Quic),
            ) {
                (Some(Protocol::QuicV1), true) => true,
                _ => false,
            },
            _ => false,
        }
    }

    /// Add one or more known addresses for peer.
    ///
    /// If peer doesn't exist, it will be added to known peers.
    ///
    /// Returns the number of added addresses after non-supported transports were filtered out.
    pub fn add_known_address(
        &mut self,
        peer: &PeerId,
        addresses: impl Iterator<Item = Multiaddr>,
    ) -> usize {
        let mut peers = self.peers.write();
        let addresses = addresses
            .filter_map(|address| {
                self.supported_transport(&address)
                    .then_some(AddressRecord::from_multiaddr(address)?)
            })
            .collect::<HashSet<_>>();

        // if all of the added addresses belonged to unsupported transports, exit early
        let num_added = addresses.len();
        if num_added == 0 {
            tracing::debug!(
                target: LOG_TARGET,
                ?peer,
                "didn't add any addreseses for peer because transport is not supported",
            );

            return 0usize;
        }

        match peers.get_mut(&peer) {
            Some(context) =>
                for record in addresses {
                    if !context.addresses.contains(record.address()) {
                        context.addresses.insert(record);
                    }
                },
            None => {
                peers.insert(
                    *peer,
                    PeerContext {
                        state: PeerState::Disconnected { dial_record: None },
                        addresses: AddressStore::from_iter(addresses.into_iter()),
                        secondary_connection: None,
                    },
                );
            }
        }

        num_added
    }

    /// Dial peer using `PeerId`.
    ///
    /// Returns an error if the peer is unknown or the peer is already connected.
    // TODO: this must report some tokent to the caller so `DialFailure` can be reported to them
    pub async fn dial(&self, peer: &PeerId) -> crate::Result<()> {
        {
            match self.peers.read().get(&peer) {
                Some(PeerContext {
                    state: PeerState::Connected { .. },
                    ..
                }) => return Err(Error::AlreadyConnected),
                Some(PeerContext {
                    state: PeerState::Disconnected { dial_record },
                    addresses,
                    ..
                }) => {
                    if addresses.is_empty() {
                        return Err(Error::NoAddressAvailable(*peer));
                    }

                    // peer is already being dialed, don't dial again until the first dial concluded
                    if dial_record.is_some() {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?dial_record,
                            "peer is aready being dialed",
                        );
                        return Ok(());
                    }
                }
                Some(PeerContext {
                    state: PeerState::Dialing { .. },
                    ..
                }) => return Ok(()),
                None => return Err(Error::PeerDoesntExist(*peer)),
            }
        }

        self.cmd_tx
            .send(InnerTransportManagerCommand::DialPeer { peer: *peer })
            .await
            .map_err(From::from)
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub async fn dial_address(&self, address: Multiaddr) -> crate::Result<()> {
        if !address.iter().any(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
            return Err(Error::AddressError(AddressError::PeerIdMissing));
        }

        self.cmd_tx
            .send(InnerTransportManagerCommand::DialAddress { address })
            .await
            .map_err(From::from)
    }
}

// TODO: add getters for these
pub struct TransportHandle {
    pub keypair: Keypair,
    pub tx: Sender<TransportManagerEvent>,
    pub rx: Receiver<TransportManagerCommand>,
    pub protocols: HashMap<ProtocolName, ProtocolContext>,
    pub next_connection_id: Arc<AtomicUsize>,
    pub next_substream_id: Arc<AtomicUsize>,
    pub protocol_names: Vec<ProtocolName>,
    pub bandwidth_sink: BandwidthSink,
    pub executor: Arc<dyn Executor>,
}

impl TransportHandle {
    pub fn protocol_set(&self, connection_id: ConnectionId) -> ProtocolSet {
        ProtocolSet::new(
            self.keypair.clone(),
            connection_id,
            self.tx.clone(),
            self.next_substream_id.clone(),
            self.protocols.clone(),
        )
    }

    /// Get next connection ID.
    pub fn next_connection_id(&mut self) -> ConnectionId {
        let connection_id = self.next_connection_id.fetch_add(1usize, Ordering::Relaxed);

        ConnectionId::from(connection_id)
    }

    pub async fn _report_connection_established(
        &mut self,
        connection: ConnectionId,
        peer: PeerId,
        endpoint: Endpoint,
    ) {
        let _ = self
            .tx
            .send(TransportManagerEvent::ConnectionEstablished {
                connection,
                peer,
                endpoint,
            })
            .await;
    }

    /// Report to `Litep2p` that a peer disconnected.
    pub async fn _report_connection_closed(&mut self, peer: PeerId, connection: ConnectionId) {
        let _ = self.tx.send(TransportManagerEvent::ConnectionClosed { peer, connection }).await;
    }

    /// Report to `Litep2p` that dialing a remote peer failed.
    pub async fn report_dial_failure(
        &mut self,
        connection: ConnectionId,
        address: Multiaddr,
        error: Error,
    ) {
        tracing::debug!(target: LOG_TARGET, ?connection, ?address, ?error, "dial failure");

        match address.iter().last() {
            Some(Protocol::P2p(hash)) => match PeerId::from_multihash(hash) {
                Ok(peer) =>
                    for (_, context) in &self.protocols {
                        let _ = context
                            .tx
                            .send(InnerTransportEvent::DialFailure {
                                peer,
                                address: address.clone(),
                            })
                            .await;
                    },
                Err(error) => {
                    tracing::warn!(target: LOG_TARGET, ?address, ?error, "failed to parse `PeerId` from `Multiaddr`");
                    debug_assert!(false);
                }
            },
            _ => {
                tracing::warn!(target: LOG_TARGET, ?address, "address doesn't contain `PeerId`");
                debug_assert!(false);
            }
        }

        let _ = self
            .tx
            .send(TransportManagerEvent::DialFailure {
                connection,
                address,
                error,
            })
            .await;
    }
}

impl Stream for TransportHandle {
    type Item = TransportManagerCommand;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::mpsc::channel;

    use super::*;

    fn make_transport_manager_handle() -> (
        TransportManagerHandle,
        Receiver<InnerTransportManagerCommand>,
    ) {
        let (cmd_tx, cmd_rx) = channel(64);

        (
            TransportManagerHandle {
                cmd_tx,
                peers: Default::default(),
                supported_transport: HashSet::new(),
            },
            cmd_rx,
        )
    }

    #[tokio::test]
    async fn tcp_and_websocket_supported() {
        let (mut handle, _rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);
        handle.supported_transport.insert(SupportedTransport::WebSocket);

        let address =
            "/dns4/google.com/tcp/24928/ws/p2p/12D3KooWKrUnV42yDR7G6DewmgHtFaVCJWLjQRi2G9t5eJD3BvTy"
                .parse()
                .unwrap();
        assert!(handle.supported_transport(&address));
    }
}
