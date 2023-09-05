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

//! Protocol-related defines.

use crate::{
    codec::ProtocolCodec,
    error::Error,
    substream::Substream,
    types::{protocol::ProtocolName, SubstreamId},
    PeerId,
};

use multiaddr::Multiaddr;

use std::fmt::Debug;

pub(crate) use connection::Permit;
pub(crate) use protocol_set::{InnerTransportEvent, ProtocolCommand, ProtocolSet};

pub use protocol_set::TransportService;

pub mod libp2p;
pub mod mdns;
pub mod notification;
pub mod request_response;

mod connection;
mod protocol_set;

/// Substream direction.
#[derive(Debug, Copy, Clone)]
pub enum Direction {
    /// Substream was opened by the remote peer.
    Inbound,

    /// Substream was opened by the local peer.
    Outbound(SubstreamId),
}

/// Events emitted by one of the installed transports to protocol(s).
#[derive(Debug)]
pub enum TransportEvent {
    /// Connection established to `peer`.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Address of remote peer.
        address: Multiaddr,
    },

    /// Connection closed.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
    },

    /// Failed to dial peer.
    ///
    /// This is reported to that protocol which initiated the connection.
    DialFailure {
        /// Peer ID.
        peer: PeerId,

        /// Dialed address.
        address: Multiaddr,
    },

    /// Substream opened for `peer`.
    SubstreamOpened {
        /// Peer ID.
        peer: PeerId,

        /// Protocol name.
        ///
        /// One protocol handler may handle multiple sub-protocols (such as `/ipfs/identify/1.0.0`
        /// and `/ipfs/identify/push/1.0.0`) or it may have aliases which should be handled by
        /// the same protocol handler. When the substream is sent from transport to the protocol
        /// handler, the protocol name that was used to negotiate the substream is also sent so
        /// the protocol can handle the substream appropriately.
        protocol: ProtocolName,

        /// Fallback protocol.
        fallback: Option<ProtocolName>,

        /// Substream direction.
        ///
        /// Informs the protocol whether the substream is inbound (opened by the remote node)
        /// or outbound (opened by the local node). This allows the protocol to distinguish
        /// between the two types of substreams and execute correct code for the substream.
        ///
        /// Outbound substreams also contain the substream ID which allows the protocol to
        /// distinguish between different outbound substreams.
        direction: Direction,

        /// Substream.
        substream: Box<dyn Substream>,
    },

    /// Failed to open substream.
    ///
    /// Substream open failures are reported only for outbound substreams.
    SubstreamOpenFailure {
        /// Substream ID.
        substream: SubstreamId,

        /// Error that occurred when the substream was being opened.
        error: Error,
    },
}

/// Transport service.
///
/// Provides an interfaces for (`Litep2p`)[crate::Litep2p] protocols to interact with the underlying
/// transport protocols.
#[async_trait::async_trait]
pub trait Transport {
    /// Dial `peer` using `PeerId`.
    ///
    /// Call fails if `Litep2p` doesn't know have a known address for the peer.
    async fn dial(&mut self, peer: &PeerId) -> crate::Result<()>;

    /// Dial peer using a `Multiaddr`.
    ///
    /// Call fails if the address is not in correct format or it contains an unsupported/disabled
    /// transport.
    ///
    /// Calling this function is only necessary for those addresses that are discovered out-of-band
    /// since `Litep2p` internally keeps track of all peer addresses it has learned through user
    /// calling this function, Kademlia peer discoveries and `Identify` responses.
    async fn dial_address(&mut self, address: Multiaddr) -> crate::Result<()>;

    /// Add known one or more addresses for peer.
    ///
    /// The list is filtered for duplicates and unsupported transports.
    fn add_known_address(&mut self, peer: &PeerId, addresses: impl Iterator<Item = Multiaddr>);

    /// Open substream to `peer`.
    ///
    /// Call fails if there is no open connection to the peer.
    async fn open_substream(&mut self, peer: PeerId) -> crate::Result<SubstreamId>;

    /// Get next [`TransportEvent`].
    async fn next_event(&mut self) -> Option<TransportEvent>;
}

/// Trait defining the interface for a user protocol.
// TODO: make `TransportService` generic?
#[async_trait::async_trait]
pub trait UserProtocol: Send + Debug {
    /// Get user protocol name.
    fn protocol(&self) -> ProtocolName;

    /// Get user protocol codec.
    fn codec(&self) -> ProtocolCodec;

    /// Start the the user protocol event loop.
    async fn run(self: Box<Self>, service: TransportService) -> crate::Result<()>;
}
