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
    error::ImmediateDialError,
    executor::Executor,
    protocol::ProtocolSet,
    transport::manager::{
        address::{AddressRecord, AddressStore},
        types::{PeerContext, PeerState, SupportedTransport},
        ProtocolContext, TransportManagerEvent, LOG_TARGET,
    },
    types::{protocol::ProtocolName, ConnectionId},
    BandwidthSink, PeerId,
};

use multiaddr::{Multiaddr, Protocol};
use parking_lot::RwLock;
use tokio::sync::mpsc::{error::TrySendError, Sender};

use std::{
    collections::{HashMap, HashSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

/// Inner commands sent from [`TransportManagerHandle`] to
/// [`crate::transport::manager::TransportManager`].
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

/// Handle for communicating with [`crate::transport::manager::TransportManager`].
#[derive(Debug, Clone)]
pub struct TransportManagerHandle {
    /// Local peer ID.
    local_peer_id: PeerId,

    /// Peers.
    peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,

    /// TX channel for sending commands to [`crate::transport::manager::TransportManager`].
    cmd_tx: Sender<InnerTransportManagerCommand>,

    /// Supported transports.
    supported_transport: HashSet<SupportedTransport>,

    /// Local listen addresess.
    listen_addresses: Arc<RwLock<HashSet<Multiaddr>>>,
}

impl TransportManagerHandle {
    /// Create new [`TransportManagerHandle`].
    pub fn new(
        local_peer_id: PeerId,
        peers: Arc<RwLock<HashMap<PeerId, PeerContext>>>,
        cmd_tx: Sender<InnerTransportManagerCommand>,
        supported_transport: HashSet<SupportedTransport>,
        listen_addresses: Arc<RwLock<HashSet<Multiaddr>>>,
    ) -> Self {
        Self {
            peers,
            cmd_tx,
            local_peer_id,
            listen_addresses,
            supported_transport,
        }
    }

    /// Register new transport to [`TransportManagerHandle`].
    pub(crate) fn register_transport(&mut self, transport: SupportedTransport) {
        self.supported_transport.insert(transport);
    }

    /// Check if `address` is supported by one of the enabled transports.
    pub fn supported_transport(&self, address: &Multiaddr) -> bool {
        let mut iter = address.iter();

        match iter.next() {
            Some(Protocol::Ip4(address)) =>
                if address.is_unspecified() {
                    return false;
                },
            Some(Protocol::Ip6(address)) =>
                if address.is_unspecified() {
                    return false;
                },
            Some(Protocol::Dns(_)) | Some(Protocol::Dns4(_)) | Some(Protocol::Dns6(_)) => {}
            _ => return false,
        }

        match iter.next() {
            None => false,
            Some(Protocol::Tcp(_)) => match iter.next() {
                Some(Protocol::P2p(_)) =>
                    self.supported_transport.contains(&SupportedTransport::Tcp),
                #[cfg(feature = "websocket")]
                Some(Protocol::Ws(_)) =>
                    self.supported_transport.contains(&SupportedTransport::WebSocket),
                #[cfg(feature = "websocket")]
                Some(Protocol::Wss(_)) =>
                    self.supported_transport.contains(&SupportedTransport::WebSocket),
                _ => false,
            },
            #[cfg(feature = "quic")]
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

    /// Check if the address is a local listen address and if so, discard it.
    fn is_local_address(&self, address: &Multiaddr) -> bool {
        let address: Multiaddr = address
            .iter()
            .take_while(|protocol| !std::matches!(protocol, Protocol::P2p(_)))
            .collect();

        self.listen_addresses.read().contains(&address)
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
                (self.supported_transport(&address) && !self.is_local_address(&address))
                    .then_some(AddressRecord::from_multiaddr(address)?)
            })
            .collect::<HashSet<_>>();

        // if all of the added addresses belonged to unsupported transports, exit early
        let num_added = addresses.len();
        if num_added == 0 {
            tracing::debug!(
                target: LOG_TARGET,
                ?peer,
                "didn't add any addresses for peer because transport is not supported",
            );

            return 0usize;
        }

        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?addresses,
            "add known addresses",
        );

        match peers.get_mut(peer) {
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
                        addresses: AddressStore::from_iter(addresses),
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
    pub fn dial(&self, peer: &PeerId) -> Result<(), ImmediateDialError> {
        if peer == &self.local_peer_id {
            return Err(ImmediateDialError::TriedToDialSelf);
        }

        {
            match self.peers.read().get(peer) {
                Some(PeerContext {
                    state: PeerState::Connected { .. },
                    ..
                }) => return Err(ImmediateDialError::AlreadyConnected),
                Some(PeerContext {
                    state: PeerState::Disconnected { dial_record },
                    addresses,
                    ..
                }) => {
                    if addresses.is_empty() {
                        return Err(ImmediateDialError::NoAddressAvailable);
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
                    state: PeerState::Dialing { .. } | PeerState::Opening { .. },
                    ..
                }) => return Ok(()),
                None => return Err(ImmediateDialError::NoAddressAvailable),
            }
        }

        self.cmd_tx
            .try_send(InnerTransportManagerCommand::DialPeer { peer: *peer })
            .map_err(|error| match error {
                TrySendError::Full(_) => ImmediateDialError::ChannelClogged,
                TrySendError::Closed(_) => ImmediateDialError::TaskClosed,
            })
    }

    /// Dial peer using `Multiaddr`.
    ///
    /// Returns an error if address it not valid.
    pub fn dial_address(&self, address: Multiaddr) -> Result<(), ImmediateDialError> {
        if !address.iter().any(|protocol| std::matches!(protocol, Protocol::P2p(_))) {
            return Err(ImmediateDialError::PeerIdMissing);
        }

        self.cmd_tx
            .try_send(InnerTransportManagerCommand::DialAddress { address })
            .map_err(|error| match error {
                TrySendError::Full(_) => ImmediateDialError::ChannelClogged,
                TrySendError::Closed(_) => ImmediateDialError::TaskClosed,
            })
    }
}

// TODO: add getters for these
pub struct TransportHandle {
    pub keypair: Keypair,
    pub tx: Sender<TransportManagerEvent>,
    pub protocols: HashMap<ProtocolName, ProtocolContext>,
    pub next_connection_id: Arc<AtomicUsize>,
    pub next_substream_id: Arc<AtomicUsize>,
    pub bandwidth_sink: BandwidthSink,
    pub executor: Arc<dyn Executor>,
}

impl TransportHandle {
    pub fn protocol_set(&self, connection_id: ConnectionId) -> ProtocolSet {
        ProtocolSet::new(
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use multihash::Multihash;
    use tokio::sync::mpsc::{channel, Receiver};

    fn make_transport_manager_handle() -> (
        TransportManagerHandle,
        Receiver<InnerTransportManagerCommand>,
    ) {
        let (cmd_tx, cmd_rx) = channel(64);

        (
            TransportManagerHandle {
                local_peer_id: PeerId::random(),
                cmd_tx,
                peers: Default::default(),
                supported_transport: HashSet::new(),
                listen_addresses: Default::default(),
            },
            cmd_rx,
        )
    }

    #[tokio::test]
    async fn tcp_supported() {
        let (mut handle, _rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);

        let address =
            "/dns4/google.com/tcp/24928/p2p/12D3KooWKrUnV42yDR7G6DewmgHtFaVCJWLjQRi2G9t5eJD3BvTy"
                .parse()
                .unwrap();
        assert!(handle.supported_transport(&address));
    }

    #[cfg(feature = "websocket")]
    #[tokio::test]
    async fn websocket_supported() {
        let (mut handle, _rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::WebSocket);

        let address =
            "/dns4/google.com/tcp/24928/ws/p2p/12D3KooWKrUnV42yDR7G6DewmgHtFaVCJWLjQRi2G9t5eJD3BvTy"
                .parse()
                .unwrap();
        assert!(handle.supported_transport(&address));
    }

    #[test]
    fn transport_not_supported() {
        let (handle, _rx) = make_transport_manager_handle();

        // only peer id (used by Polkadot sometimes)
        assert!(!handle.supported_transport(
            &Multiaddr::empty().with(Protocol::P2p(Multihash::from(PeerId::random())))
        ));

        // only one transport
        assert!(!handle.supported_transport(
            &Multiaddr::empty().with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
        ));

        // any udp-based protocol other than quic
        assert!(!handle.supported_transport(
            &Multiaddr::empty()
                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                .with(Protocol::Udp(8888))
                .with(Protocol::Utp)
        ));

        // any other protocol other than tcp
        assert!(!handle.supported_transport(
            &Multiaddr::empty()
                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                .with(Protocol::Sctp(8888))
        ));
    }

    #[test]
    fn zero_addresses_added() {
        let (mut handle, _rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);

        assert!(
            handle.add_known_address(
                &PeerId::random(),
                vec![
                    Multiaddr::empty()
                        .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                        .with(Protocol::Udp(8888))
                        .with(Protocol::Utp),
                    Multiaddr::empty()
                        .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                        .with(Protocol::Tcp(8888))
                        .with(Protocol::Wss(std::borrow::Cow::Owned("/".to_string()))),
                ]
                .into_iter()
            ) == 0usize
        );
    }

    #[tokio::test]
    async fn dial_already_connected_peer() {
        let (mut handle, _rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);

        let peer = {
            let peer = PeerId::random();
            let mut peers = handle.peers.write();

            peers.insert(
                peer,
                PeerContext {
                    state: PeerState::Connected {
                        record: AddressRecord::from_multiaddr(
                            Multiaddr::empty()
                                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                                .with(Protocol::Tcp(8888))
                                .with(Protocol::P2p(Multihash::from(peer))),
                        )
                        .unwrap(),
                        dial_record: None,
                    },
                    secondary_connection: None,
                    addresses: AddressStore::from_iter(
                        vec![Multiaddr::empty()
                            .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                            .with(Protocol::Tcp(8888))
                            .with(Protocol::P2p(Multihash::from(peer)))]
                        .into_iter(),
                    ),
                },
            );
            drop(peers);

            peer
        };

        match handle.dial(&peer) {
            Err(Error::AlreadyConnected) => {}
            _ => panic!("invalid return value"),
        }
    }

    #[tokio::test]
    async fn peer_already_being_dialed() {
        let (mut handle, _rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);

        let peer = {
            let peer = PeerId::random();
            let mut peers = handle.peers.write();

            peers.insert(
                peer,
                PeerContext {
                    state: PeerState::Dialing {
                        record: AddressRecord::from_multiaddr(
                            Multiaddr::empty()
                                .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                                .with(Protocol::Tcp(8888))
                                .with(Protocol::P2p(Multihash::from(peer))),
                        )
                        .unwrap(),
                    },
                    secondary_connection: None,
                    addresses: AddressStore::from_iter(
                        vec![Multiaddr::empty()
                            .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                            .with(Protocol::Tcp(8888))
                            .with(Protocol::P2p(Multihash::from(peer)))]
                        .into_iter(),
                    ),
                },
            );
            drop(peers);

            peer
        };

        match handle.dial(&peer) {
            Ok(()) => {}
            _ => panic!("invalid return value"),
        }
    }

    #[tokio::test]
    async fn no_address_available_for_peer() {
        let (mut handle, _rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);

        let peer = {
            let peer = PeerId::random();
            let mut peers = handle.peers.write();

            peers.insert(
                peer,
                PeerContext {
                    state: PeerState::Disconnected { dial_record: None },
                    secondary_connection: None,
                    addresses: AddressStore::new(),
                },
            );
            drop(peers);

            peer
        };

        match handle.dial(&peer) {
            Err(Error::NoAddressAvailable(failed_peer)) => {
                assert_eq!(failed_peer, peer);
            }
            _ => panic!("invalid return value"),
        }
    }

    #[tokio::test]
    async fn pending_connection_for_disconnected_peer() {
        let (mut handle, mut rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);

        let peer = {
            let peer = PeerId::random();
            let mut peers = handle.peers.write();

            peers.insert(
                peer,
                PeerContext {
                    state: PeerState::Disconnected {
                        dial_record: Some(
                            AddressRecord::from_multiaddr(
                                Multiaddr::empty()
                                    .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                                    .with(Protocol::Tcp(8888))
                                    .with(Protocol::P2p(Multihash::from(peer))),
                            )
                            .unwrap(),
                        ),
                    },
                    secondary_connection: None,
                    addresses: AddressStore::from_iter(
                        vec![Multiaddr::empty()
                            .with(Protocol::Ip4(std::net::Ipv4Addr::new(127, 0, 0, 1)))
                            .with(Protocol::Tcp(8888))
                            .with(Protocol::P2p(Multihash::from(peer)))]
                        .into_iter(),
                    ),
                },
            );
            drop(peers);

            peer
        };

        match handle.dial(&peer) {
            Ok(()) => {}
            _ => panic!("invalid return value"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn try_to_dial_self() {
        let (mut handle, mut rx) = make_transport_manager_handle();
        handle.supported_transport.insert(SupportedTransport::Tcp);

        match handle.dial(&handle.local_peer_id) {
            Err(Error::TriedToDialSelf) => {}
            _ => panic!("invalid return value"),
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn is_local_address() {
        let (cmd_tx, _cmd_rx) = channel(64);

        let handle = TransportManagerHandle {
            local_peer_id: PeerId::random(),
            cmd_tx,
            peers: Default::default(),
            supported_transport: HashSet::new(),
            listen_addresses: Arc::new(RwLock::new(HashSet::from_iter([
                "/ip6/::1/tcp/8888".parse().expect("valid multiaddress"),
                "/ip4/127.0.0.1/tcp/8888".parse().expect("valid multiaddress"),
                "/ip6/::1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                    .parse()
                    .expect("valid multiaddress"),
                "/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                    .parse()
                    .expect("valid multiaddress"),
            ]))),
        };

        // local addresses
        assert!(handle.is_local_address(
            &"/ip6/::1/tcp/8888".parse::<Multiaddr>().expect("valid multiaddress")
        ));
        assert!(handle
            .is_local_address(&"/ip4/127.0.0.1/tcp/8888".parse().expect("valid multiaddress")));
        assert!(handle.is_local_address(
            &"/ip6/::1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        ));
        assert!(handle.is_local_address(
            &"/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWT2ouvz5uMmCvHJGzAGRHiqDts5hzXR7NdoQ27pGdzp9Q"
                .parse()
                .expect("valid multiaddress")
        ));

        // same address but different peer id
        assert!(handle.is_local_address(
            &"/ip6/::1/tcp/8888/p2p/12D3KooWPGxxxQiBEBZ52RY31Z2chn4xsDrGCMouZ88izJrak2T1"
                .parse::<Multiaddr>()
                .expect("valid multiaddress")
        ));
        assert!(handle.is_local_address(
            &"/ip4/127.0.0.1/tcp/8888/p2p/12D3KooWPGxxxQiBEBZ52RY31Z2chn4xsDrGCMouZ88izJrak2T1"
                .parse()
                .expect("valid multiaddress")
        ));

        // different address
        assert!(!handle
            .is_local_address(&"/ip4/127.0.0.1/tcp/9999".parse().expect("valid multiaddress")));
        // different address
        assert!(!handle
            .is_local_address(&"/ip4/127.0.0.1/tcp/7777".parse().expect("valid multiaddress")));
    }
}
