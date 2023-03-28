// Copyright 2023 litep2p developers
// TODO: add copyright from libp2p
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
    protocol::libp2p::{Libp2pProtocol, Libp2pProtocolEvent},
    transport::TransportEvent,
    DEFAULT_CHANNEL_SIZE,
};

use tokio::sync::mpsc::{channel, Receiver, Sender};

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::ping";

/// Events emitted by [`IpfsPing`].
pub enum PingEvent {}

/// IPFS Ping protocol handler.
pub struct IpfsPing {
    /// TX channel for sending `Libp2pProtocolEvent`s.
    event_tx: Sender<Libp2pProtocolEvent>,

    /// RX channel for receiving `TransportEvent`s.
    transport_rx: Receiver<TransportEvent>,
}

impl IpfsPing {
    /// Create new [`IpfsPing`] object.
    pub fn new(
        event_tx: Sender<Libp2pProtocolEvent>,
        transport_rx: Receiver<TransportEvent>,
    ) -> Self {
        Self {
            event_tx,
            transport_rx,
        }
    }

    /// [`IpfsPing`] event loop.
    async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "start ipfs ping event loop");

        while let Some(event) = self.transport_rx.recv().await {
            match event {
                TransportEvent::SubstreamOpened(protocol, peer, substream) => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "ipfs ping substream opened");
                }
                _ => {}
            }
        }
    }
}

impl Libp2pProtocol for IpfsPing {
    fn start(transport_rx: Receiver<TransportEvent>) -> Receiver<Libp2pProtocolEvent> {
        let (event_tx, event_rx) = channel(DEFAULT_CHANNEL_SIZE);

        tokio::spawn(IpfsPing::new(event_tx, transport_rx).run());
        event_rx
    }
}
