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

//! TCP transport types.

use crate::{peer_id::PeerId, transport::Connection};

use futures::{stream::FuturesUnordered, Stream as FuturesStream};
use multiaddr::Multiaddr;
use tokio::{net::TcpStream, sync::mpsc::Receiver};
use yamux::{Control, Stream};

use std::{future::Future, io::Error, pin::Pin};

/// Type representing pending outbound connections.
pub type PendingConnections =
    FuturesUnordered<Pin<Box<dyn Future<Output = Result<TcpStream, Error>> + Send>>>;

/// Type representing pending negotiations.
pub type PendingNegotiations =
    FuturesUnordered<Pin<Box<dyn Future<Output = crate::Result<ConnectionContext>> + Send>>>;

/// Type representing incoming substreams.
pub type IncomingSubstreams =
    FuturesUnordered<Pin<Box<dyn Future<Output = Option<(PeerId, Stream)>> + Send>>>;

/// Type representing incoming substreams.
pub type PendingOutboundSubstreams = FuturesUnordered<
    Pin<Box<dyn Future<Output = (String, PeerId, crate::Result<Box<dyn Connection>>)> + Send>>,
>;

/// TCP transport events.
#[derive(Debug)]
pub enum TcpTransportEvent {
    /// Open connection to remote peer.
    OpenConnection(Multiaddr),

    /// Close connection to remote peer.
    CloseConnection(PeerId),

    /// Open substream to remote peer.
    OpenSubstream(String, PeerId, Vec<u8>),
}

/// Context returned to [`crate::transport::tcp::TcpTransport`] after the negotation of protocols
/// have finished.
pub struct ConnectionContext {
    /// Peer ID of remote.
    pub peer: PeerId,

    /// `yamux` controller.
    pub control: Control,
}

impl ConnectionContext {
    /// Create new [`ConnectionContext`].
    pub fn new(peer: PeerId, control: Control) -> Self {
        Self { peer, control }
    }
}
