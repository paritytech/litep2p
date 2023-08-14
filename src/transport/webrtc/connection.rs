// Copyright 2022 Parity Technologies (UK) Ltd.
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

use futures::stream::FuturesUnordered;
use futures::{
    channel::{
        mpsc,
        oneshot::{self, Sender},
    },
    lock::Mutex as FutMutex,
    StreamExt,
    {future::BoxFuture, ready},
};
use webrtc::data::data_channel::DataChannel as DetachedDataChannel;
use webrtc::data_channel::RTCDataChannel;
use webrtc::peer_connection::RTCPeerConnection;

use std::sync::Arc;

/// Maximum number of unprocessed data channels.
/// See [`Connection::poll_inbound`].
const MAX_DATA_CHANNELS_IN_FLIGHT: usize = 10;

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc::connection";

/// WebRTC connection.
pub struct WebRtcConnection {
    /// [`RTCPeerConnection`] to the remote peer.
    ///
    /// Uses futures mutex because used in async code (see poll_outbound and poll_close).
    peer_conn: Arc<FutMutex<RTCPeerConnection>>,

    /// Channel onto which incoming data channels are put.
    incoming_data_channels_rx: mpsc::Receiver<Arc<DetachedDataChannel>>,
}

impl WebRtcConnection {
    /// Create new [`WebRtcConnection`]
    pub async fn new(connection: RTCPeerConnection) -> Self {
        let (data_channel_tx, data_channel_rx) = mpsc::channel(MAX_DATA_CHANNELS_IN_FLIGHT);

        WebRtcConnection::register_incoming_data_channels_handler(
            &connection,
            Arc::new(FutMutex::new(data_channel_tx)),
        )
        .await;

        Self {
            peer_conn: Arc::new(FutMutex::new(connection)),
            incoming_data_channels_rx: data_channel_rx,
        }
    }

    // TODO: refactor this code
    async fn register_incoming_data_channels_handler(
        connection: &RTCPeerConnection,
        tx: Arc<FutMutex<mpsc::Sender<Arc<DetachedDataChannel>>>>,
    ) {
        connection.on_data_channel(Box::new(move |data_channel: Arc<RTCDataChannel>| {
            tracing::debug!(target: LOG_TARGET, id = ?data_channel.id(), "incoming data channel");

            let tx = tx.clone();

            Box::pin(async move {
                data_channel.on_open({
                    let data_channel = data_channel.clone();
                    Box::new(move || {
                        tracing::debug!(
                            target: LOG_TARGET,
                            id = ?data_channel.id(),
                            "data channel open",
                        );

                        Box::pin(async move {
                            let data_channel = data_channel.clone();
                            let id = data_channel.id();
                            match data_channel.detach().await {
                                Ok(detached) => {
                                    let mut tx = tx.lock().await;
                                    if let Err(error) = tx.try_send(detached.clone()) {
                                        tracing::error!(
                                            target: LOG_TARGET,
                                            ?id,
                                            ?error,
                                            "failed to send data channel",
                                        );

                                        // We're not accepting data channels fast enough =>
                                        // close this channel.
                                        //
                                        // Ideally we'd refuse to accept a data channel
                                        // during the negotiation process, but it's not
                                        // possible with the current API.
                                        if let Err(error) = detached.close().await {
                                            tracing::error!(
                                                target: LOG_TARGET,
                                                ?id,
                                                ?error,
                                                "failed to close data channel",
                                            );
                                        }
                                    }
                                }
                                Err(error) => {
                                    tracing::error!(
                                        target: LOG_TARGET,
                                        ?id,
                                        ?error,
                                        "cannot detach data channel",
                                    );
                                }
                            };
                        })
                    })
                });
            })
        }));
    }

    pub async fn run(mut self) -> crate::Result<()> {
        loop {
            match self.incoming_data_channels_rx.next().await {
                Some(channel) => {
                    tracing::error!(target: LOG_TARGET, "do something with data channel");
                }
                None => {
                    panic!("failure")
                }
            }
        }
    }
}
