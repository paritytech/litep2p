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

//! This example demonstrates how to implement a custom protocol for litep2p.

use litep2p::{
    codec::ProtocolCodec,
    config::Litep2pConfigBuilder,
    protocol::{Direction, TransportService, UserProtocol},
    protocol::{Transport, TransportEvent},
    types::protocol::ProtocolName,
    Litep2p, PeerId,
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc::{channel, Receiver, Sender},
};

use std::collections::HashMap;

/// Events received from the protocol.
#[derive(Debug)]
enum CustomProtocolEvent {
    /// Received `message` from `peer`.
    MessageReceived {
        /// Peer ID.
        peer: PeerId,

        /// Message.
        message: Vec<u8>,
    },
}

/// Commands sent to the protocol.
#[derive(Debug)]
enum CustomProtocolCommand {
    /// Send `message` to `peer`.
    SendMessage {
        /// Peer ID.
        peer: PeerId,

        /// Message.
        message: Vec<u8>,
    },
}

/// Handle for communicating with the protocol.
#[derive(Debug)]
struct CustomProtocolHandle {
    cmd_tx: Sender<CustomProtocolCommand>,
    event_rx: Receiver<CustomProtocolEvent>,
}

#[derive(Debug)]
struct CustomProtocol {
    cmd_rx: Receiver<CustomProtocolCommand>,
    event_tx: Sender<CustomProtocolEvent>,
    peers: HashMap<PeerId, Option<Vec<u8>>>,
}

impl CustomProtocol {
    /// Create new [`CustomProtocol`].
    pub fn new() -> (Self, CustomProtocolHandle) {
        let (event_tx, event_rx) = channel(64);
        let (cmd_tx, cmd_rx) = channel(64);

        (
            Self {
                cmd_rx,
                event_tx,
                peers: HashMap::new(),
            },
            CustomProtocolHandle { cmd_tx, event_rx },
        )
    }
}

#[async_trait::async_trait]
impl UserProtocol for CustomProtocol {
    fn protocol(&self) -> ProtocolName {
        ProtocolName::from("/custom-protocol/1")
    }

    // Protocol code is set to `Unspecified` which means that `litep2p` won't provide
    // `Sink + Stream` for the protocol and instead only `AsyncWrite + AsyncRead` are provided.
    // User must implement their custom codec on top of `Substream` using, e.g.,
    // `tokio_codec::Framed` if they want to have message framing.
    fn codec(&self) -> ProtocolCodec {
        ProtocolCodec::Unspecified
    }

    /// Start running event loop for [`CustomProtocol`].
    async fn run(mut self: Box<Self>, mut service: TransportService) -> litep2p::Result<()> {
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(CustomProtocolCommand::SendMessage { peer, message }) => {
                        // protocol only allows sending messages of exactly 10 bytes
                        if message.len() != 10 {
                            println!("message to {peer:?} has invalid length, got {}, expected 10", message.len());
                        }

                        // if the peer doesn't exist in the protocol, we don't have a connection
                        // open so dial them and save the message.
                        if !self.peers.contains_key(&peer) {
                            match service.dial(&peer).await {
                                Ok(_) => {
                                    self.peers.insert(peer, Some(message));
                                }
                                Err(error) => {
                                    println!("failed to dial {peer:?}: {error:?}");
                                }
                            }
                        }
                    }
                    None => return Err(litep2p::Error::EssentialTaskClosed),
                },
                event = service.next_event() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        // connection established to peer
                        //
                        // check if the peer already exist in the protocol with a pending message
                        // and if yes, open substream to the peer.
                        match self.peers.get(&peer) {
                            Some(Some(_)) => {
                                if let Err(error) = service.open_substream(peer) {
                                    println!("failed to open substream to {peer:?}: {error:?}");
                                }
                            }
                            Some(None) => {}
                            None => {
                                self.peers.insert(peer, None);
                            }
                        }
                    }
                    Some(TransportEvent::SubstreamOpened { peer, mut substream, direction, .. }) => {
                        match direction {
                            // inbound substream, attempt to read exactly 10 bytes and send the received message to user
                            Direction::Inbound => {
                                let mut buffer = vec![0u8; 10];

                                // NOTE: it's not good idea to block the protocol like this and reading/writing from
                                // the substream should be done in a non-blocking way, e.g., with `FuturesUnordered`.
                                match substream.read_exact(&mut buffer).await {
                                    Ok(_) => {
                                        self.event_tx.send(CustomProtocolEvent::MessageReceived {
                                            peer,
                                            message: buffer,
                                        }).await.unwrap();
                                    }
                                    Err(error) => {
                                        println!("failed to read data from {peer:?}: {error:?}");
                                    }
                                }
                            }
                            Direction::Outbound(_) => {
                                // outbound substream was opened so message must exist
                                let mut message = self.peers.get_mut(&peer).expect("peer to exist").take().unwrap();

                                // NOTE: it's not good idea to block the protocol like this and reading/writing from
                                // the substream should be done in a non-blocking way, e.g., with `FuturesUnordered`.
                                if let Err(error) = substream.write_all(&mut message).await {
                                    println!("failed to send message to {peer:?}: {error:?}");
                                }
                            }
                        }
                    }
                    None => return Err(litep2p::Error::EssentialTaskClosed),
                    _ => {},
                }
            }
        }
    }
}

fn make_litep2p() -> (Litep2p, CustomProtocolHandle) {
    let (custom_protocol, handle) = CustomProtocol::new();

    (
        Litep2p::new(
            Litep2pConfigBuilder::new()
                .with_tcp(Default::default())
                .with_user_protocol(Box::new(custom_protocol))
                .build(),
        )
        .unwrap(),
        handle,
    )
}

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let (mut litep2p1, handle1) = make_litep2p();
    let (mut litep2p2, mut handle2) = make_litep2p();

    let peer2 = *litep2p2.local_peer_id();
    let listen_address = litep2p2.listen_addresses().next().unwrap().clone();
    litep2p1.add_known_address(peer2, std::iter::once(listen_address));

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = litep2p1.next_event() => {}
                _ = litep2p2.next_event() => {}
            }
        }
    });

    // send message to peer2
    handle1
        .cmd_tx
        .send(CustomProtocolCommand::SendMessage {
            peer: peer2,
            message: vec![1u8; 10],
        })
        .await
        .unwrap();

    let CustomProtocolEvent::MessageReceived { peer, message } =
        handle2.event_rx.recv().await.unwrap();

    println!("received message from {peer:?}: {message:?}");
}
