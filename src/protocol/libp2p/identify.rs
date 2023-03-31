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
    crypto::ed25519::Keypair,
    protocol::libp2p::{Libp2pProtocol, Libp2pProtocolEvent},
    transport::{Connection, TransportEvent},
    DEFAULT_CHANNEL_SIZE,
};

use asynchronous_codec::{Framed, FramedParts};
use bytes::BytesMut;
use futures::{AsyncReadExt, AsyncWriteExt, SinkExt};
use prost::Message;
use tokio::sync::mpsc::{channel, Receiver, Sender};
use unsigned_varint::{
    codec::UviBytes,
    decode::{self, Error},
    encode,
};

/// Log target for the file.
const LOG_TARGET: &str = "ipfs::identify";

/// IPFS Identify protocol name
pub const PROTOCOL_NAME: &str = "/ipfs/id/1.0.0";

/// IPFS Identify push protocol name.
pub const PUSH_PROTOCOL_NAME: &str = "/ipfs/id/push/1.0.0";

/// Size for `/ipfs/ping/1.0.0` payloads.
// TODO: what is the max size?
const IDENTIFY_PAYLOAD_SIZE: usize = 8192;

mod identify_schema {
    include!(concat!(env!("OUT_DIR"), "/identify.rs"));
}

pub enum IdentifyEvent {}

pub struct IpfsIdentify {
    /// TX channel for sending `Libp2pProtocolEvent`s.
    event_tx: Sender<Libp2pProtocolEvent>,

    /// RX channel for receiving `TransportEvent`s.
    transport_rx: Receiver<TransportEvent>,

    /// Read buffer for incoming messages.
    read_buffer: Vec<u8>,
}

impl IpfsIdentify {
    pub fn new(
        event_tx: Sender<Libp2pProtocolEvent>,
        transport_rx: Receiver<TransportEvent>,
    ) -> Self {
        Self {
            event_tx,
            transport_rx,
            read_buffer: vec![0u8; IDENTIFY_PAYLOAD_SIZE],
        }
    }

    /// [`IpfsIdentify`] event loop.
    async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "start ipfs ping event loop");

        while let Some(event) = self.transport_rx.recv().await {
            match event {
                TransportEvent::SubstreamOpened(protocol, peer, mut substream) => {
                    tracing::trace!(target: LOG_TARGET, ?peer, "ipfs identify substream opened");

                    let keypair = Keypair::generate().public();

                    // TODO: fill with proper info
                    let identify = identify_schema::Identify {
                        protocol_version: None,
                        agent_version: None,
                        public_key: Some(
                            crate::crypto::PublicKey::Ed25519(keypair).to_protobuf_encoding(),
                        ),
                        listen_addrs: vec!["/ip6/::1/tcp/8888"
                            .parse::<multiaddr::Multiaddr>()
                            .expect("valid multiaddress")
                            .to_vec()],
                        observed_addr: None,
                        protocols: vec![],
                    };
                    let mut msg = Vec::with_capacity(identify.encoded_len());
                    identify
                        .encode(&mut msg)
                        .expect("`msg` to have enough capacity");

                    // why is this using unsigned-varint?
                    let mut decoder = Framed::new(substream, UviBytes::<bytes::Bytes>::default());
                    decoder.send(BytesMut::from(&msg[..]).into()).await.unwrap();
                }
                event => {
                    tracing::info!(target: LOG_TARGET, ?event, "ignoring `TransportEvent`");
                }
            }
        }
    }
}

impl Libp2pProtocol for IpfsIdentify {
    // Initialize [`IpfsIdentify`] and starts its event loop.
    fn start(event_tx: Sender<Libp2pProtocolEvent>) -> Sender<TransportEvent> {
        let (transport_tx, transport_rx) = channel(DEFAULT_CHANNEL_SIZE);

        tokio::spawn(IpfsIdentify::new(event_tx, transport_rx).run());
        transport_tx
    }
}
