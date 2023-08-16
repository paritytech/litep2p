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
        webrtc::{
            certificate::Certificate,
            config::Config,
            udp_mux::{UDPMuxEvent, UDPMuxNewAddr},
        },
        Transport, TransportCommand, TransportContext,
    },
};

use multiaddr::{Multiaddr, Protocol};
use tokio::sync::mpsc::Receiver;

use std::net::{IpAddr, SocketAddr};

// TODO: merge some of these
mod certificate;
mod config;
mod connection;
mod error;
mod fingerprint;
mod req_res_chan;
mod sdp;
mod substream;
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

/// Logging target for the file.
const LOG_TARGET: &str = "webrtc";

/// WebRTC transport configuration.
// TODO: move to config
#[derive(Debug)]
pub struct WebRtcTransportConfig {
    /// Listen address.
    listen_address: Multiaddr,
}

/// WebRTC transport.
pub struct WebRtcTransport {
    /// Listen address.
    listen_address: SocketAddr,

    /// Muxed UPD socket.
    socket: UDPMuxNewAddr,

    /// WebRTC config.
    webrtc_config: Config,

    /// Transport context.
    context: TransportContext,
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
                    "invalid protocol, expected `WebRtc`"
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
}

#[async_trait::async_trait]
impl Transport for WebRtcTransport {
    type Config = WebRtcTransportConfig;

    /// Create new [`Transport`] object.
    async fn new(
        context: TransportContext,
        config: Self::Config,
        rx: Receiver<TransportCommand>,
    ) -> crate::Result<Self> {
        let (listen_address, _) = Self::get_socket_address(&config.listen_address)?;
        let socket = UDPMuxNewAddr::listen_on(listen_address.clone()).unwrap();

        let mut rng = rand::thread_rng();
        let certificate = Certificate::generate(&mut rng).expect("valid certificate");
        let config = Config::new(context.keypair.clone(), certificate.clone());

        Ok(Self {
            listen_address: socket.listen_addr(),
            webrtc_config: config,
            context,
            socket,
        })
    }

    /// Get assigned listen address.
    fn listen_address(&self) -> Multiaddr {
        Multiaddr::empty()
            .with(Protocol::from(self.listen_address.ip()))
            .with(Protocol::Udp(self.listen_address.port()))
            .with(Protocol::WebRTC)
            .with(Protocol::Certhash(
                self.webrtc_config.fingerprint.to_multihash(),
            ))
            .with(Protocol::P2p(
                PeerId::from(PublicKey::Ed25519(self.context.keypair.public())).into(),
            ))
    }

    /// Start transport event loop.
    async fn start(mut self) -> crate::Result<()> {
        loop {
            match futures::future::poll_fn(|cx| self.socket.poll(cx)).await {
                UDPMuxEvent::NewAddr(address) => {
                    tracing::debug!(target: LOG_TARGET, "new inbound connection received");
                }
                UDPMuxEvent::Error(error) => {
                    tracing::warn!(
                        target: LOG_TARGET,
                        ?error,
                        "socket error, closing transport"
                    );
                    return Err(Error::IoError(error.kind()));
                }
            }
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
    use std::collections::HashMap;
    use tokio::sync::mpsc::channel;

    #[tokio::test]
    async fn webrtc_testbench() {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .try_init();

        let keypair = Keypair::generate();
        let (tx1, _rx1) = channel(64);
        let (tx2, _rx2) = channel(64);
        let (event_tx, mut event_rx) = channel(64);
        let (_command_tx, command_rx) = channel(64);

        let context = TransportContext {
            local_peer_id: PeerId::random(),
            tx: event_tx,
            keypair: keypair.clone(),
            protocols: HashMap::from_iter([
                (
                    ProtocolName::from("/ipfs/ping/1.0.0"),
                    ProtocolInfo {
                        tx: tx1,
                        codec: ProtocolCodec::Identity(32),
                    },
                ),
                (
                    ProtocolName::from("/980e7cbafbcd37f8cb17be82bf8d53fa81c9a588e8a67384376e862da54285dc/block-announces/1"),
                    ProtocolInfo {
                        tx: tx2,
                        codec: ProtocolCodec::UnsignedVarint,
                    },
                ),
            ]),
        };
        let transport_config = WebRtcTransportConfig {
            listen_address: "/ip4/192.168.1.173/udp/8888/webrtc-direct".parse().unwrap(),
        };
        let transport = WebRtcTransport::new(context, transport_config, command_rx)
            .await
            .unwrap();

        tracing::info!("{}", transport.listen_address());

        transport.start().await;
    }
}
