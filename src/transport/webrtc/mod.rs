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
    crypto::PublicKey,
    error::{AddressError, Error},
    peer_id::PeerId,
    transport::{
        webrtc::connection::WebRtcConnection, Transport, TransportCommand, TransportContext,
    },
    types::ConnectionId,
};

use multiaddr::{multihash::Multihash, Multiaddr, Protocol};
use str0m::{
    change::{DtlsCert, IceCreds},
    channel::{ChannelConfig, ChannelId},
    net::{DatagramRecv, Receive},
    Candidate, Input, Rtc,
};
use tokio::{
    net::UdpSocket,
    sync::mpsc::{channel, Receiver, Sender},
};

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

mod connection;
mod handshake;
mod util;

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc";

/// Hardcoded remote fingerprint.
const REMOTE_FINGERPRINT: &str =
    "sha-256 FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF";

#[derive(Debug)]
pub struct WebRtcTransportConfig {
    /// WebRTC listening address.
    pub listen_address: Multiaddr,
}

/// WebRTC transport.
pub(crate) struct WebRtcTransport {
    /// Transport context.
    context: TransportContext,

    /// UDP socket.
    socket: Arc<UdpSocket>,

    /// DTLS certificate.
    dtls_cert: DtlsCert,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Next connection id.
    next_connection_id: ConnectionId,

    /// Connected peers.
    peers: HashMap<SocketAddr, Sender<Vec<u8>>>,

    /// RX channel for receiving commands from `Litep2p`.
    rx: Receiver<TransportCommand>,
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
            .set_dtls_certification(self.dtls_cert.clone())
            .set_certificate_fingerprint_verification(false)
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
    async fn on_socket_input(&mut self, source: SocketAddr, buffer: Vec<u8>) -> crate::Result<()> {
        // if the `Rtc` object already exists for `souce`, pass the message directly to that connection.
        if let Some(tx) = self.peers.get_mut(&source) {
            tracing::debug!(target: LOG_TARGET, ?source, "send input to peer");
            return tx.send(buffer).await.map_err(From::from);
        }

        // if the peer doesn't exist, decode the message and expect to receive `Stun`
        // so that a new connection can be initialized
        let contents: DatagramRecv = buffer
            .as_slice()
            .try_into()
            .map_err(|_| Error::InvalidData)?;

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

                    let (mut rtc, noise_channel_id) = self.make_rtc_client(
                        ufrag,
                        pass,
                        source,
                        self.socket.local_addr().unwrap(),
                    );

                    let input = Input::Receive(
                        Instant::now(),
                        Receive {
                            source,
                            destination: self.socket.local_addr().unwrap(),
                            contents: DatagramRecv::Stun(message.clone()),
                        },
                    );

                    match rtc.accepts(&input) {
                        true => rtc
                            .handle_input(input)
                            .expect("client to handle input successfully"),
                        false => panic!("client to accept input"),
                    }

                    let (tx, rx) = channel(64);
                    let connection = WebRtcConnection::new(
                        rtc,
                        self.next_connection_id.next(),
                        noise_channel_id,
                        self.context.keypair.clone(),
                        self.context.clone(),
                        source,
                        self.listen_address,
                        Arc::clone(&self.socket),
                        rx,
                    );

                    tokio::spawn(async move {
                        if let Err(error) = connection.run().await {
                            tracing::error!(target: LOG_TARGET, ?error, "connection failed");
                        }
                    });

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
impl Transport for WebRtcTransport {
    type Config = WebRtcTransportConfig;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self>
    where
        Self: Sized,
    {
        tracing::info!(
            target: LOG_TARGET,
            listen_address = ?config.listen_address,
            "start webrtc transport",
        );

        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let socket = UdpSocket::bind(listen_address).await?;
        let listen_address = socket.local_addr()?;
        let dtls_cert = DtlsCert::new();

        Ok(Self {
            rx,
            context,
            dtls_cert,
            listen_address,
            peers: HashMap::new(),
            socket: Arc::new(socket),
            next_connection_id: ConnectionId::new(),
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        let fingerprint = self.dtls_cert.fingerprint().bytes;

        const MULTIHASH_SHA256_CODE: u64 = 0x12;
        let certificate = Multihash::wrap(MULTIHASH_SHA256_CODE, &fingerprint)
            .expect("fingerprint's len to be 32 bytes");

        Multiaddr::empty()
            .with(Protocol::from(self.listen_address.ip()))
            .with(Protocol::Udp(self.listen_address.port()))
            .with(Protocol::WebRTC)
            .with(Protocol::Certhash(certificate))
            .with(Protocol::P2p(
                PeerId::from(PublicKey::Ed25519(self.context.keypair.public())).into(),
            ))
    }

    /// Start transport event loop.
    async fn start(mut self) -> crate::Result<()> {
        loop {
            // // Clean out disconnected clients
            // self.peers.retain(|_, (_, c)| c.rtc.is_alive());

            // let mut to_propagate = Vec::new();
            // for (_, (_, client)) in self.peers.iter_mut() {
            //     // TODO: poll output polls the rtc client but also the substreams and context
            //     let value = client.poll_output().await;
            //     to_propagate.push(value);
            // }

            // let timeouts: Vec<_> = Vec::new();

            // // We keep propagating client events until all clients respond with a timeout.
            // if to_propagate.len() > timeouts.len() {
            //     // Start over to propagate more client data until all are timeouts.
            //     continue;
            // }

            // // Timeout in case we have no clients. We can't wait forever since we need to keep
            // // polling the spawn_new_clients to discover a client.
            // fn default_timeout() -> Instant {
            //     Instant::now() + Duration::from_millis(100)
            // }

            // // // All poll_output resulted in timeouts, figure out the shortest timeout.
            // let timeout = timeouts.into_iter().min().unwrap_or_else(default_timeout);

            // // The read timeout is not allowed to be 0. In case it is 0, we set 1 millisecond.
            // let duration = (timeout - Instant::now()).max(Duration::from_millis(1));

            let mut buf = vec![0; 2000];

            tokio::select! {
                result = self.socket.recv_from(&mut buf) => match result {
                    Ok((n, source)) => {
                        buf.truncate(n);

                        if let Err(error) = self.on_socket_input(source, buf).await {
                            tracing::error!(target: LOG_TARGET, ?error, "failed to handle input");
                        }

                    }
                    Err(error) => panic!("error: {error:?}"), // TODO: don't panic
                },
                event = self.rx.recv() => match event {
                    Some(_event) => {}
                    None => return Err(Error::EssentialTaskClosed),
                },
                // _ = tokio::time::sleep(duration) => {
                //     // None
                // }
            }

            // // drive time forward in all clients.
            // let now = Instant::now();
            // for (_, (_, client)) in self.peers.iter_mut() {
            //     client.handle_input(Input::Timeout(now));
            // }
        }
    }
}

// TODO: remove
/// Events propagated between client.
#[allow(clippy::large_enum_variant)]
enum WebRtcEvent {
    /// When we have nothing to propagate.
    Noop,

    /// Poll client has reached timeout.
    Timeout(Instant),
}

impl WebRtcEvent {
    /// If the propagated data is a timeout, returns the instant.
    fn as_timeout(&self) -> Option<Instant> {
        if let Self::Timeout(v) = self {
            Some(*v)
        } else {
            None
        }
    }
}
