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

use crate::transport::{Transport, TransportCommand, TransportContext};

use multiaddr::Multiaddr;
use tokio::sync::mpsc::Receiver;

mod certificate;
mod config;
mod connection;
mod error;
mod fingerprint;
mod req_res_chan;
mod sdp;
mod udp_mux;
mod upgrade;
mod util;

mod schema {
    pub(super) mod webrtc {
        include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
    }

    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

#[derive(Debug)]
pub struct WebRtcTransportConfig {}

pub struct WebRtcTransport {}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    type Config = WebRtcTransportConfig;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self> {
        todo!();
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        todo!();
    }

    /// Start transport event loop.
    async fn start(mut self) -> crate::Result<()> {
        todo!();
    }
}
