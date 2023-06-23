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
    peer_id::PeerId,
    protocol::request_response::types::Config,
    protocol::{ConnectionEvent, ConnectionService, Direction},
    substream::Substream,
    types::SubstreamId,
    TransportService,
};

pub mod types;

/// Logging target for the file.
const LOG_TARGET: &str = "request-response";

/// Request-response protocol.
pub(crate) struct RequestResponseProtocol {
    /// Transport service.
    service: TransportService,
}

impl RequestResponseProtocol {
    /// Create new [`RequestResponseProtocol`].
    pub(crate) fn new(service: TransportService, _config: Config) -> Self {
        Self { service }
    }

    /// Connection established to remote peer.
    async fn on_connection_established(
        &mut self,
        peer: PeerId,
        _service: ConnectionService,
    ) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection established");

        todo!();
    }

    /// Connection closed to remote peer.
    fn on_connection_closed(&mut self, peer: PeerId) {
        tracing::debug!(target: LOG_TARGET, ?peer, "connection closed");

        todo!();
    }

    /// Handle outbound substream.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        _substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(
            target: LOG_TARGET,
            ?peer,
            ?substream_id,
            "handle outbound substream"
        );

        todo!();
    }

    /// Substream opened to remote peer.
    async fn on_inbound_substream(
        &mut self,
        peer: PeerId,
        _substream: Box<dyn Substream>,
    ) -> crate::Result<()> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound substream");

        todo!();
    }

    /// Failed to open substream to remote peer.
    fn on_substream_open_failure(&mut self, peer: PeerId, error: Error) {
        tracing::debug!(
            target: LOG_TARGET,
            ?peer,
            ?error,
            "failed to open substream"
        );
    }

    /// Start [`RequestResponseProtocol`] event loop.
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                event = self.service.next_event() => match event {
                    Some(ConnectionEvent::ConnectionEstablished { peer, service }) => {
                        if let Err(error) = self.on_connection_established(peer, service).await {
                            tracing::debug!(
                                target: LOG_TARGET,
                                ?peer,
                                ?error,
                                "failed to register peer",
                            );
                        }
                    }
                    Some(ConnectionEvent::ConnectionClosed { peer }) => {
                        self.on_connection_closed(peer);
                    }
                    Some(ConnectionEvent::SubstreamOpened {
                        peer,
                        substream,
                        direction,
                        ..
                    }) => match direction {
                        Direction::Inbound => {
                            if let Err(error) = self.on_inbound_substream(peer, substream).await {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to handle inbound substream",
                                );
                            }
                        }
                        Direction::Outbound(substream_id) => {
                            if let Err(error) = self
                                .on_outbound_substream(peer, substream_id, substream)
                                .await
                            {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?peer,
                                    ?error,
                                    "failed to handle outbound substream",
                                );
                            }
                        }
                    },
                    Some(ConnectionEvent::SubstreamOpenFailure { peer, error }) => {
                        self.on_substream_open_failure(peer, error);
                    }
                    None => return,
                }
            }
        }
    }
}
