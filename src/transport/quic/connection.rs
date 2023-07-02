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

use s2n_quic::connection::Connection;

/// Logging target for the file.
const LOG_TARGET: &str = "quic::connection";

/// QUIC connection.
pub(crate) struct QuicConnection {
    /// Inner QUIC connection.
    connection: Connection,

    /// Connection ID.
    connection_id: usize,
}

impl QuicConnection {
    /// Create new [`QuiConnection`].
    pub(crate) fn new(connection: Connection, connection_id: usize) -> Self {
        Self {
            connection,
            connection_id,
        }
    }

    /// Start [`QuicConnection`] event loop.
    pub(crate) async fn start(mut self) -> crate::Result<()> {
        tracing::debug!(target: LOG_TARGET, "starting quic connection handler");

        loop {
            tokio::select! {
                substream = self.connection.accept_bidirectional_stream() => match substream {
                    Ok(Some(stream)) => {
                        // TODO: call multistream-select
                        todo!();
                    }
                    Ok(None) => todo!(),
                    Err(error) => todo!(),
                }
            }
        }
    }
}
