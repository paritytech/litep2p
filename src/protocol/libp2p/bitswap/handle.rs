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

use crate::{types::RequestId, PeerId};

use tokio::sync::mpsc::Receiver;

use std::{
    pin::Pin,
    task::{Context, Poll},
};

/// Events emitted by the bitswap protocol.
#[derive(Debug)]
pub enum BitswapEvent {
    /// Bitswap request.
    Request,

    /// Bitswap response.
    Response,
}

/// Handle for communicating with the bitswap protocol.
pub struct BitswapHandle {
    /// RX channel for receiving bitswap events.
    event_rx: Receiver<BitswapEvent>,
}

impl BitswapHandle {
    /// Create new [`BitswapHandle`].
    pub(super) fn new(event_rx: Receiver<BitswapEvent>) -> Self {
        Self { event_rx }
    }

    /// Send `request` to `peer`.
    pub async fn send_request(&self, peer: PeerId, request: Vec<u8>) {
        todo!();
    }

    /// Send `response` to `peer`.
    pub async fn send_response(&self, request_id: RequestId, response: Vec<u8>) {
        todo!();
    }
}

impl futures::Stream for BitswapHandle {
    type Item = BitswapEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.event_rx).poll_recv(cx)
    }
}
