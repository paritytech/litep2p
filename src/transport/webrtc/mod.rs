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
    channel::ChannelConfig,
    net::{self, DatagramRecv, Receive},
    Candidate, Input, Rtc,
};
use tokio::{net::UdpSocket, sync::mpsc::Receiver};

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

mod connection;

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc";

#[derive(Debug)]
pub struct WebRtcTransportConfig {
    pub listen_address: Multiaddr,
}

/// WebRTC transport.
pub(crate) struct WebRtcTransport {
    /// Transport context.
    context: TransportContext,

    /// UDP socket.
    socket: UdpSocket,

    /// DTLS certificate.
    dtls_cert: DtlsCert,

    /// Assigned listen addresss.
    listen_address: SocketAddr,

    /// Next connection id.
    next_connection_id: ConnectionId,

    /// RX channel for receiving commands from `Litep2p`.
    _rx: Receiver<TransportCommand>,
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
}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    type Config = WebRtcTransportConfig;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        _rx: Receiver<TransportCommand>,
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
            _rx,
            context,
            socket,
            dtls_cert,
            listen_address,
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
        let mut clients: HashMap<SocketAddr, WebRtcConnection> = HashMap::new();
        let mut buf = vec![0; 2000];

        loop {
            // Clean out disconnected clients
            clients.retain(|_, c| c.rtc.is_alive());

            let mut to_propagate = Vec::new();
            for (_, client) in clients.iter_mut() {
                let value = client.poll_output(&self.socket).await;
                to_propagate.push(value);
            }

            let timeouts: Vec<_> = to_propagate.iter().filter_map(|p| p.as_timeout()).collect();

            // We keep propagating client events until all clients respond with a timeout.
            if to_propagate.len() > timeouts.len() {
                propagate(&mut clients, to_propagate);
                // Start over to propagate more client data until all are timeouts.
                continue;
            }

            // Timeout in case we have no clients. We can't wait forever since we need to keep
            // polling the spawn_new_clients to discover a client.
            fn default_timeout() -> Instant {
                Instant::now() + Duration::from_millis(100)
            }

            // All poll_output resulted in timeouts, figure out the shortest timeout.
            let timeout = timeouts.into_iter().min().unwrap_or_else(default_timeout);

            // The read timeout is not allowed to be 0. In case it is 0, we set 1 millisecond.
            let duration = (timeout - Instant::now()).max(Duration::from_millis(1));

            if let Some(input) = read_socket_input(&self.socket, &mut buf, duration).await {
                match input {
                    Input::Timeout(_) => {}
                    Input::Receive(
                        _,
                        net::Receive {
                            source,
                            destination,
                            contents: DatagramRecv::Stun(message),
                        },
                    ) => {
                        if let Some((u, p)) = message.split_username() {
                            tracing::debug!(target: LOG_TARGET, "Received STUN from {}:{}", u, p);

                            if !clients.contains_key(&source) {
                                let mut rtc = Rtc::builder()
                                    .set_ice_lite(true)
                                    .set_dtls_certification(self.dtls_cert.clone())
                                    .set_certificate_fingerprint_verification(false)
                                    .build();
                                rtc.add_local_candidate(Candidate::host(destination).unwrap());
                                rtc.add_remote_candidate(Candidate::host(source).unwrap());
                                rtc.direct_api().set_remote_fingerprint("sha-256 FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF:FF".parse().unwrap());
                                rtc.direct_api().set_remote_ice_credentials(IceCreds {
                                    ufrag: u.to_owned(),
                                    pass: p.to_owned(),
                                });
                                rtc.direct_api().set_local_ice_credentials(IceCreds {
                                    ufrag: u.to_owned(),
                                    pass: p.to_owned(),
                                });
                                rtc.direct_api().set_ice_controlling(false);
                                rtc.direct_api().start_dtls(false).unwrap();
                                rtc.direct_api().start_sctp(false);

                                let noise_channel_id =
                                    rtc.direct_api().create_data_channel(ChannelConfig {
                                        label: "noise".to_string(),
                                        ordered: false,
                                        reliability: Default::default(),
                                        negotiated: Some(0),
                                        protocol: "".to_string(),
                                    });

                                clients.insert(
                                    source,
                                    WebRtcConnection::new(
                                        rtc,
                                        self.next_connection_id.next(),
                                        noise_channel_id,
                                        self.context.keypair.clone(),
                                        self.context.clone(),
                                        source,
                                    ),
                                );
                            }

                            match clients.get_mut(&source) {
                                Some(client) => {
                                    if client.rtc.accepts(&Input::Receive(
                                        Instant::now(),
                                        net::Receive {
                                            source,
                                            destination,
                                            contents: DatagramRecv::Stun(message.clone()),
                                        },
                                    )) {
                                        client
                                            .rtc
                                            .handle_input(Input::Receive(
                                                Instant::now(),
                                                net::Receive {
                                                    source,
                                                    destination,
                                                    contents: DatagramRecv::Stun(message),
                                                },
                                            ))
                                            .unwrap();
                                    }
                                }
                                None => panic!("client does not exist"),
                            }
                        }
                    }
                    other => {
                        for (_, c) in clients.iter_mut() {
                            if c.accepts(&other) {
                                c.handle_input(other);
                                break;
                            }
                        }
                    }
                }
            }

            // Drive time forward in all clients.
            let now = Instant::now();
            for (_, client) in &mut clients {
                client.handle_input(Input::Timeout(now));
            }
        }
    }
}

fn propagate(clients: &mut HashMap<SocketAddr, WebRtcConnection>, to_propagate: Vec<Propagated>) {
    for p in to_propagate {
        let Some(client_id) = p.client_id() else {
            // If the event doesn't have a client id, it can't be propagated,
            // (it's either a noop or a timeout).
            continue;
        };

        for (_, client) in &mut *clients {
            if client.connection_id == client_id {
                // Do not propagate to originating client.
                continue;
            }

            match &p {
                Propagated::Noop | Propagated::Timeout(_) => {}
            }
        }
    }
}

async fn read_socket_input<'a>(
    socket: &UdpSocket,
    buf: &'a mut Vec<u8>,
    duration: Duration,
) -> Option<Input<'a>> {
    buf.resize(2000, 0);

    tokio::select! {
        result = socket.recv_from(buf) => match result {
            Ok((n, source)) => {
                buf.truncate(n);

                // Parse data to a DatagramRecv, which help preparse network data to
                // figure out the multiplexing of all protocols on one UDP port.
                let Ok(contents) = buf.as_slice().try_into() else {
                    return None;
                };

                Some(Input::Receive(
                    Instant::now(),
                    Receive {
                        source,
                        destination: socket.local_addr().unwrap(),
                        contents,
                    },
                ))
            }
            Err(error) => panic!("error: {error:?}"),
        },
        _ = tokio::time::sleep(duration) => {
            None
        }
    }
}

/// Events propagated between client.
#[allow(clippy::large_enum_variant)]
enum Propagated {
    /// When we have nothing to propagate.
    Noop,

    /// Poll client has reached timeout.
    Timeout(Instant),
}

impl Propagated {
    /// Get client id, if the propagated event has a client id.
    fn client_id(&self) -> Option<ConnectionId> {
        None
    }

    /// If the propagated data is a timeout, returns the instant.
    fn as_timeout(&self) -> Option<Instant> {
        if let Self::Timeout(v) = self {
            Some(*v)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        codec::ProtocolCodec, crypto::ed25519::Keypair, protocol::ProtocolInfo,
        types::protocol::ProtocolName,
    };
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn create_transport() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair = Keypair::generate();
        let (tx, _rx) = channel(64);
        let (event_tx, _event_rx) = channel(64);
        let (_command_tx, command_rx) = channel(64);

        let context = TransportContext {
            tx: event_tx,
            keypair: keypair.clone(),
            protocols: HashMap::from_iter([
                (
                    ProtocolName::from("/ipfs/ping/1.0.0"),
                    ProtocolInfo {
                        tx: tx.clone(),
                        codec: ProtocolCodec::Identity(32),
                    },
                ),
                (
                    ProtocolName::from("/980e7cbafbcd37f8cb17be82bf8d53fa81c9a588e8a67384376e862da54285dc/block-announces/1"),
                    ProtocolInfo {
                        tx,
                        codec: ProtocolCodec::Identity(32),
                    },
                ),
            ]),
        };
        let transport_config = WebRtcTransportConfig {
            listen_address: "/ip4/192.168.1.173/udp/8888".parse().unwrap(),
        };

        let transport = WebRtcTransport::new(context, transport_config, command_rx)
            .await
            .unwrap();

        tracing::error!(target: "webrtc", "listen address: {}", transport.listen_address());

        transport.start().await.unwrap();
    }
}
