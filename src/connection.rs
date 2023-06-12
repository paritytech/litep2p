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
    peer_id::PeerId,
    substream::{Substream, SubstreamSet},
    transport::ConnectionNew,
    types::protocol::ProtocolName,
};

use futures::Stream;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;

use std::fmt::Debug;

/// Service provided by an open connection to protocols.
#[async_trait::async_trait]
pub trait ConnectionService: Send {
    /// Open substream for `protocol`.
    async fn open_substream(&self, protocol: ProtocolName) -> crate::Result<()>;

    /// Try to open a substream for `protocol` and if the call would block, return an error.
    fn try_open_substream(&self, protocol: ProtocolName) -> crate::Result<()>;
}

/// Events emitted by [`Connection`] to protocol handlers.
pub enum ConnectionEvent<S: Substream> {
    /// Connection established to remote peer.
    ConnectionEstablished {
        /// Peer ID.
        peer: PeerId,

        /// Connection event sender.
        tx: Box<dyn ConnectionService>,
    },

    /// Connection closed to remote peer.
    ConnectionClosed {
        /// Peer ID.
        peer: PeerId,
    },

    /// Substream opened by `peer`.
    SubstreamOpened {
        /// Peer ID.
        peer: PeerId,

        /// Opened substream.
        substream: S,
    },
}

/// Litep2p connection object.
pub struct Connection<C: ConnectionNew> {
    connection: C,
}

impl<C: ConnectionNew> Connection<C> {
    /// Create new [`Connection`] object.
    pub fn start(connection: C) {
        tokio::spawn(async move {
            let mut connection = Self { connection };
            connection.run().await
        });
    }

    async fn run(&mut self) -> crate::Result<()> {
        todo!();
    }
}
