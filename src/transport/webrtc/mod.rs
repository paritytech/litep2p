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

//! WebRTC transport.

#![allow(unused)]

use crate::{
    error::{AddressError, Error},
    transport::{
        manager::{TransportHandle, TransportManagerCommand},
        webrtc::{config::TransportConfig, connection::WebRtcConnection},
        Transport, TransportBuilder, TransportEvent,
    },
    types::ConnectionId,
    PeerId,
};

use futures::{Stream, StreamExt};
use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use str0m::{
    change::{DtlsCert, IceCreds},
    channel::{ChannelConfig, ChannelId},
    net::{DatagramRecv, Receive},
    Candidate, Input, Rtc,
};
use tokio::{
    io::ReadBuf,
    net::UdpSocket,
    sync::mpsc::{channel, Sender},
};

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

pub mod config;

mod connection;
mod substream;
mod util;

mod schema {
    pub(super) mod webrtc {
        include!(concat!(env!("OUT_DIR"), "/webrtc.rs"));
    }

    pub(super) mod noise {
        include!(concat!(env!("OUT_DIR"), "/noise.rs"));
    }
}

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p::webrtc";

/// Hardcoded remote fingerprint.
const REMOTE_FINGERPRINT: &str =
    "sha-256 FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF";

/// WebRTC transport.
pub(crate) struct WebRtcTransport {
    /// Transport context.
    context: TransportHandle,

    /// UDP socket.
    socket: Arc<UdpSocket>,

    /// DTLS certificate.
    dtls_cert: DtlsCert,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Connected peers.
    peers: HashMap<SocketAddr, Sender<Vec<u8>>>,
}

impl WebRtcTransport {
    /// Extract socket address and `PeerId`, if found, from `address`.
    fn get_socket_address(address: &Multiaddr) -> crate::Result<(SocketAddr, Option<PeerId>)> {
        tracing::trace!(target: LOG_TARGET, ?address, "parse multi address");

        let mut iter = address.iter();
        let socket_address = match iter.next() {
            Some(Protocol::Ip6(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V6(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Upd`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            Some(Protocol::Ip4(address)) => match iter.next() {
                Some(Protocol::Udp(port)) => SocketAddr::new(IpAddr::V4(address), port),
                protocol => {
                    tracing::error!(
                        target: LOG_TARGET,
                        ?protocol,
                        "invalid transport protocol, expected `Udp`",
                    );
                    return Err(Error::AddressError(AddressError::InvalidProtocol));
                }
            },
            protocol => {
                tracing::error!(target: LOG_TARGET, ?protocol, "invalid transport protocol");
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        match iter.next() {
            Some(Protocol::WebRTC) => {}
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `WebRTC`"
                );
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        }

        let maybe_peer = match iter.next() {
            Some(Protocol::P2p(multihash)) => Some(PeerId::from_multihash(multihash)?),
            None => None,
            protocol => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?protocol,
                    "invalid protocol, expected `P2p` or `None`"
                );
                return Err(Error::AddressError(AddressError::InvalidProtocol));
            }
        };

        Ok((socket_address, maybe_peer))
    }

    /// Create RTC client and open channel for Noise handshake.
    fn make_rtc_client(
        &self,
        ufrag: &str,
        pass: &str,
        source: SocketAddr,
        destination: SocketAddr,
    ) -> (Rtc, ChannelId) {
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_dtls_cert(self.dtls_cert.clone())
            .set_fingerprint_verification(false)
            .build();
        rtc.add_local_candidate(Candidate::host(destination).unwrap());
        rtc.add_remote_candidate(Candidate::host(source).unwrap());
        rtc.direct_api()
            .set_remote_fingerprint(REMOTE_FINGERPRINT.parse().expect("parse() to succeed"));
        rtc.direct_api().set_remote_ice_credentials(IceCreds {
            ufrag: ufrag.to_owned(),
            pass: pass.to_owned(),
        });
        rtc.direct_api().set_local_ice_credentials(IceCreds {
            ufrag: ufrag.to_owned(),
            pass: pass.to_owned(),
        });
        rtc.direct_api().set_ice_controlling(false);
        rtc.direct_api().start_dtls(false).unwrap();
        rtc.direct_api().start_sctp(false);

        let noise_channel_id = rtc.direct_api().create_data_channel(ChannelConfig {
            label: "noise".to_string(),
            ordered: false,
            reliability: Default::default(),
            negotiated: Some(0),
            protocol: "".to_string(),
        });

        (rtc, noise_channel_id)
    }

    /// Handle socket input.
    fn on_socket_input(&mut self, source: SocketAddr, buffer: Vec<u8>) -> crate::Result<()> {
        // if the `Rtc` object already exists for `souce`, pass the message directly to that
        // connection.
        if let Some(tx) = self.peers.get_mut(&source) {
            // TODO: implement properly
            match tx.try_send(buffer) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    tracing::warn!(target: LOG_TARGET, ?error, "failed to send datagram to connection");
                    return Ok(());
                }
            }
        }

        // if the peer doesn't exist, decode the message and expect to receive `Stun`
        // so that a new connection can be initialized
        let contents: DatagramRecv =
            buffer.as_slice().try_into().map_err(|_| Error::InvalidData)?;

        match contents {
            DatagramRecv::Stun(message) => {
                if let Some((ufrag, pass)) = message.split_username() {
                    tracing::debug!(
                        target: LOG_TARGET,
                        ?source,
                        ?ufrag,
                        ?pass,
                        "received stun message"
                    );

                    // create new `Rtc` object for the peer and give it the received STUN message
                    let (mut rtc, noise_channel_id) = self.make_rtc_client(
                        ufrag,
                        pass,
                        source,
                        self.socket.local_addr().unwrap(),
                    );

                    rtc.handle_input(Input::Receive(
                        Instant::now(),
                        Receive {
                            source,
                            destination: self.socket.local_addr().unwrap(),
                            contents: DatagramRecv::Stun(message.clone()),
                        },
                    ))
                    .expect("client to handle input successfully");

                    let (tx, rx) = channel(64);
                    let connection_id = self.context.next_connection_id();

                    let connection = WebRtcConnection::new(
                        rtc,
                        connection_id,
                        noise_channel_id,
                        self.context.keypair.clone(),
                        self.context.protocol_set(connection_id),
                        source,
                        self.listen_address,
                        Arc::clone(&self.socket),
                        rx,
                    );

                    self.context.executor.run(Box::pin(async move {
                        let _ = connection.run().await;
                    }));
                    self.peers.insert(source, tx);
                }
            }
            message => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?source,
                    ?message,
                    "received unexpected message for a connection that doesn't eixst"
                );
            }
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl TransportBuilder for WebRtcTransport {
    type Config = TransportConfig;
    type Transport = WebRtcTransport;

    /// Create new [`Transport`] object.
    async fn new(context: TransportHandle, config: Self::Config) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_addresses = ?config.listen_addresses,
            "start webrtc transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_addresses[0])?;
        let socket = UdpSocket::bind(listen_address).await?;
        let listen_address = socket.local_addr()?;
        let dtls_cert = DtlsCert::new();

        Ok(Self {
            context,
            dtls_cert,
            listen_address,
            peers: HashMap::new(),
            socket: Arc::new(socket),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Vec<Multiaddr> {
        let fingerprint = self.dtls_cert.fingerprint().bytes;

        const MULTIHASH_SHA256_CODE: u64 = 0x12;
        let certificate = Multihash::wrap(MULTIHASH_SHA256_CODE, &fingerprint)
            .expect("fingerprint's len to be 32 bytes");

        vec![Multiaddr::empty()
            .with(Protocol::from(self.listen_address.ip()))
            .with(Protocol::Udp(self.listen_address.port()))
            .with(Protocol::WebRTC)
            .with(Protocol::Certhash(certificate))]
    }

    async fn start(mut self) -> crate::Result<()> {
        while let Some(event) = self.next().await {}

        Ok(())
    }
}

impl Transport for WebRtcTransport {
    fn dial(&mut self, _connection_id: ConnectionId, _address: Multiaddr) -> crate::Result<()> {
        todo!();
    }
}

impl Stream for WebRtcTransport {
    type Item = TransportEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        // TODO: optimizations
        let mut buf = vec![0u8; 16384];
        let mut read_buf = ReadBuf::new(&mut buf);

        match self.socket.poll_recv_from(cx, &mut read_buf) {
            Poll::Pending => {}
            Poll::Ready(Ok(source)) => {
                let nread = read_buf.filled().len();
                buf.truncate(nread);

                if let Err(error) = self.on_socket_input(source, buf) {
                    tracing::error!(target: LOG_TARGET, ?error, "failed to handle input");
                }
            }
            Poll::Ready(Err(error)) => {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?error,
                    "failed to read from webrtc socket",
                );

                return Poll::Ready(None);
            }
        }

        while let Poll::Ready(Some(command)) = self.context.poll_next_unpin(cx) {
            match command {
                TransportManagerCommand::Dial {
                    address,
                    connection: connection_id,
                } => {
                    return Poll::Ready(Some(TransportEvent::DialFailure {
                        connection_id,
                        address,
                        error: Error::NotSupported(format!("webrtc cannot dial peers")),
                    }));
                }
            }
        }

        Poll::Pending
    }
}

// TODO: remove
/// Events propagated between client.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum WebRtcEvent {
    /// When we have nothing to propagate.
    Noop,

    /// Poll client has reached timeout.
    Timeout(Instant),
}
