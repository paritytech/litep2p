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
    transport::{
        websocket::types::Config, Transport, TransportContext, TransportError, TransportEvent,
    },
};

use multiaddr::Multiaddr;

mod types;

pub(crate) struct WebSocketError {}

impl TransportError for WebSocketError {
    /// Get connection ID.
    fn connection_id(&self) -> Option<usize> {
        todo!();
    }

    /// Convert [`TransportError`] into `Error`
    fn into_error(self) -> Error {
        todo!();
    }
}

pub(crate) struct WebSocketTransport {}

#[async_trait::async_trait]
impl Transport for WebSocketTransport {
    type Config = Config;
    type Error = WebSocketError;

    /// Create new [`Transport`] object.
    async fn new(context: TransportContext, config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized,
    {
        todo!();
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        todo!();
    }

    /// Try to open a connection to remote peer.
    ///
    /// The result is polled using [`Transport::next_connection()`].
    fn open_connection(&mut self, address: Multiaddr) -> crate::Result<usize> {
        todo!();
    }

    /// Poll next connection.
    async fn next_event(&mut self) -> Result<TransportEvent, Self::Error> {
        todo!();
    }
}
