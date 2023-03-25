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
    config::TransportConfig,
    crypto::noise::NoiseConfiguration,
    error::Error,
    peer_id::PeerId,
    types::{ProtocolId, ProtocolType, RequestId, SubstreamId},
};

use multiaddr::Multiaddr;

pub mod tcp;

/// Supported transport types.
pub enum TransportType {
    /// TCP.
    Tcp(Multiaddr),
}

#[async_trait::async_trait]
pub trait TransportService {
    async fn open_connection(
        &mut self,
        address: Multiaddr,
        noise_config: NoiseConfiguration,
    ) -> crate::Result<PeerId>;
    fn close_connection(&mut self, peer: PeerId) -> crate::Result<()>;
    // TODO: return handle + sink for sending/receiving notifications.
    fn open_substream(
        &mut self,
        peer: PeerId,
        protocol: ProtocolId,
        handshake: Option<Vec<u8>>,
    ) -> crate::Result<SubstreamId>;
    fn close_substream(&mut self, substream: SubstreamId) -> crate::Result<()>;
    fn send_request(
        &mut self,
        peer: PeerId,
        protocol: ProtocolType,
        request: Vec<u8>,
    ) -> crate::Result<RequestId>;
    fn send_response(&mut self, request: RequestId, response: Vec<u8>) -> crate::Result<RequestId>;
}

#[async_trait::async_trait]
pub trait Transport {
    /// Start the underlying transport listener and return a handle which allows `litep2p` to
    // interact with the transport.
    fn start(config: TransportConfig) -> Box<dyn TransportService>;
}
