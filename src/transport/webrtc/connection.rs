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

use crate::{
    crypto::{ed25519::Keypair, PublicKey},
    error::Error,
    multistream_select::listener_negotiate,
    peer_id::PeerId,
    protocol::{ConnectionService, Direction, ProtocolEvent, ProtocolSet},
    substream::SubstreamType,
    transport::{
        webrtc::{
            fingerprint::Fingerprint,
            substream::WebRtcSubstream,
            udp_mux::NewAddr,
            upgrade,
            util::{self, WebRtcMessage},
        },
        TransportContext,
    },
    types::{protocol::ProtocolName, SubstreamId},
};

use futures::{
    channel::{
        mpsc,
        oneshot::{self, Sender},
    },
    lock::Mutex as FutMutex,
    stream::FuturesUnordered,
    SinkExt, StreamExt,
    {future::BoxFuture, ready},
};
use multiaddr::{Multiaddr, Protocol};
use multihash::Multihash;
use webrtc::{
    data::data_channel::DataChannel as DetachedDataChannel,
    data_channel::RTCDataChannel,
    ice::udp_mux::UDPMux,
    peer_connection::{configuration::RTCConfiguration, RTCPeerConnection},
};

use std::{net::SocketAddr, sync::Arc};

/// Maximum number of unprocessed data channels.
/// See [`Connection::poll_inbound`].
const MAX_DATA_CHANNELS_IN_FLIGHT: usize = 10;

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc::connection";

/// WebRTC connection.
pub struct WebRtcConnection {
    /// Remote peer ID.
    remote_peer_id: PeerId,

    /// [`RTCPeerConnection`] to the remote peer.
    ///
    /// Uses futures mutex because used in async code (see poll_outbound and poll_close).
    peer_conn: Arc<FutMutex<RTCPeerConnection>>,

    /// Channel onto which incoming data channels are put.
    incoming_data_channels_rx: mpsc::Receiver<Arc<DetachedDataChannel>>,

    /// Protocol set.
    protocol_set: ProtocolSet,
}

impl WebRtcConnection {
    /// Accept inbound WebRTC connection.
    ///
    /// If the negotiation succeeds, start event loop for the connection.
    pub async fn accept_connection(
        remote_info: NewAddr,
        config: RTCConfiguration,
        udp_mux: Arc<dyn UDPMux + Send + Sync>,
        server_fingerprint: Fingerprint,
        id_keys: Keypair,
        mut context: TransportContext,
    ) -> crate::Result<()> {
        tracing::debug!(
            target: LOG_TARGET,
            address = ?remote_info.addr,
            ufrag = ?remote_info.ufrag,
            "accept inbound connection"
        );

        let (remote_peer_id, connection) = upgrade::inbound(
            remote_info.addr,
            config,
            udp_mux,
            server_fingerprint,
            remote_info.ufrag.clone(),
            id_keys,
        )
        .await?;

        tracing::debug!(
            target: LOG_TARGET,
            address = ?remote_info.addr,
            ufrag = ?remote_info.ufrag,
            "connection negotiated"
        );

        let address = Multiaddr::empty()
            .with(Protocol::from(remote_info.addr.ip()))
            .with(Protocol::Udp(remote_info.addr.port()))
            .with(Protocol::WebRTC)
            .with(Protocol::Certhash(
                util::get_remote_fingerprint(&connection)
                    .await
                    .to_multihash(),
            ))
            .with(Protocol::P2p(
                Multihash::from_bytes(&remote_peer_id.to_bytes()).unwrap(),
            ));

        // TODO: this should be reported by `WebRtcTransport`
        context
            .report_connection_established(remote_peer_id, address)
            .await;
        let protocol_set = ProtocolSet::from_transport_context(remote_peer_id, context).await?;

        Self::new(remote_peer_id, connection, protocol_set)
            .await
            .run()
            .await
    }

    /// Create new [`WebRtcConnection`]
    pub async fn new(
        remote_peer_id: PeerId,
        connection: RTCPeerConnection,
        protocol_set: ProtocolSet,
    ) -> Self {
        let (data_channel_tx, data_channel_rx) = mpsc::channel(MAX_DATA_CHANNELS_IN_FLIGHT);

        util::register_incoming_data_channels_handler(
            &connection,
            Arc::new(FutMutex::new(data_channel_tx)),
        )
        .await;

        Self {
            protocol_set,
            remote_peer_id,
            peer_conn: Arc::new(FutMutex::new(connection)),
            incoming_data_channels_rx: data_channel_rx,
        }
    }

    /// Negotiate protocol for substream;
    async fn negotiate_substream(
        &mut self,
        channel: Arc<DetachedDataChannel>,
    ) -> crate::Result<(WebRtcSubstream, ProtocolName)> {
        let mut substream = WebRtcSubstream::new(channel.clone());

        let message = substream.next().await.ok_or(Error::Disconnected)??;
        let (protocol, response) =
            listener_negotiate(&mut self.protocol_set.protocols.keys(), message.freeze())?;

        substream.send(response.into()).await?;

        Ok((substream, protocol))
    }

    /// Open outbound substream.
    async fn open_substream(
        &mut self,
        protocol: ProtocolName,
        substream_id: SubstreamId,
    ) -> crate::Result<()> {
        tracing::warn!(target: LOG_TARGET, "open substream");
        Ok(())

        // let peer_conn = self.peer_conn.lock().await;

        // let data_channel = peer_conn.create_data_channel("", None).await.unwrap();

        // // No need to hold the lock during the DTLS handshake.
        // drop(peer_conn);

        // tracing::trace!(
        //     target: LOG_TARGET,
        //     "opening data channel {}",
        //     data_channel.id()
        // );

        // let (tx, rx) = oneshot::channel::<Arc<DetachedDataChannel>>();

        // // Wait until the data channel is opened and detach it.
        // util::register_data_channel_open_handler(data_channel, tx).await;

        // // Wait until data channel is opened and ready to use
        // match rx.await {
        //     Ok(detached) => Ok(detached),
        //     Err(e) => Err(Error::Internal(e.to_string())),
        // }
        // Ok(())
    }

    /// Event loop for [`WebRtcConnection`].
    pub async fn run(mut self) -> crate::Result<()> {
        loop {
            tokio::select! {
                event = self.incoming_data_channels_rx.next() => match event {
                    None => return Ok(()),
                    Some(channel) => {
                        tracing::trace!(target: LOG_TARGET, "channel opened, negotiate protocol");

                        match self.negotiate_substream(channel.clone()).await {
                            Err(error) => {
                                tracing::debug!(
                                    target: LOG_TARGET,
                                    ?error,
                                    "failed to negotiate connection"
                                );
                                let _ = channel.close();
                                continue;
                            }
                            Ok((mut substream, protocol)) => {
                                substream.apply_codec(self.protocol_set.protocol_codec(&protocol));

                                let _ = self.protocol_set.report_substream_open(
                                   self.remote_peer_id,
                                    protocol,
                                    Direction::Inbound,
                                    SubstreamType::<tokio::net::TcpStream>::Ready(Box::new(substream)),
                                ).await;
                            }
                        }
                    }
                },
                command = self.protocol_set.next_event() => match command {
                    Some(ProtocolEvent::OpenSubstream { protocol, substream_id }) => {
                        self.open_substream(protocol, substream_id).await;
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}
