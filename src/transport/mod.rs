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

//! Transport protocol implementations provided by [`Litep2p`](`crate::Litep2p`).

use crate::{transport::manager::TransportHandle, types::ConnectionId, Error, PeerId};

use futures::Stream;
use multiaddr::Multiaddr;

use std::{fmt::Debug, time::Duration};

pub mod quic;
pub mod tcp;
pub mod webrtc;
pub mod websocket;

pub(crate) mod dummy;
pub(crate) mod manager;

/// Timeout for opening a connection.
pub(crate) const CONNECTION_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// Connection endpoint.
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// Successful outbound connection.
    Dialer {
        /// Address that was dialed.
        address: Multiaddr,

        /// Connection ID.
        connection_id: ConnectionId,
    },

    /// Successful inbound connection.
    Listener {
        /// Local connection address.
        address: Multiaddr,

        /// Connection ID.
        connection_id: ConnectionId,
    },
}

impl Endpoint {
    /// Get `Multiaddr` of the [`Endpoint`].
    pub fn address(&self) -> &Multiaddr {
        match self {
            Self::Dialer { address, .. } => &address,
            Self::Listener { address, .. } => &address,
        }
    }

    /// Crate dialer.
    pub fn dialer(address: Multiaddr, connection_id: ConnectionId) -> Self {
        Endpoint::Dialer {
            address,
            connection_id,
        }
    }

    /// Create listener.
    pub fn listener(address: Multiaddr, connection_id: ConnectionId) -> Self {
        Endpoint::Listener {
            address,
            connection_id,
        }
    }

    /// Get `ConnectionId` of the `Endpoint`.
    pub fn connection_id(&self) -> ConnectionId {
        match self {
            Self::Dialer { connection_id, .. } => *connection_id,
            Self::Listener { connection_id, .. } => *connection_id,
        }
    }
}

impl Into<Multiaddr> for Endpoint {
    fn into(self) -> Multiaddr {
        match self {
            Self::Dialer { address, .. } => address,
            Self::Listener { address, .. } => address,
        }
    }
}

/// Transport event.
pub(crate) enum TransportEvent {
    /// Connection established to remote peer.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Endpoint.
        endpoint: Endpoint,
    },

    /// Connection closed to remote peer.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,

        /// Connection ID.
        connection_id: ConnectionId,
    },

    /// Failed to dial remote peer.
    DialFailure {
        /// Connection ID.
        connection_id: ConnectionId,

        /// Dialed address.
        address: Multiaddr,

        /// Error.
        error: Error,
    },
}

pub(crate) trait TransportBuilder {
    type Config: Debug;
    type Transport: Transport;

    /// Create new [`Transport`] object.
    fn new(context: TransportHandle, config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized;

    /// Get assigned listen address.
    fn listen_address(&self) -> Vec<Multiaddr>;
}

pub(crate) trait Transport: Stream + Unpin + Send {
    /// Dial `address`.
    fn dial(&mut self, connection_id: ConnectionId, address: Multiaddr) -> crate::Result<()>;

    /// Accept negotiated connection.
    fn accept(&mut self, connection_id: ConnectionId) -> crate::Result<()>;

    /// Reject negotiated connection.
    fn reject(&mut self, connection_id: ConnectionId) -> crate::Result<()>;
}
