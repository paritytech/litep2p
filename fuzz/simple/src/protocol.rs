// Copyright 2025 Security Research Labs GmbH
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

use litep2p::{
    PeerId,
    codec::ProtocolCodec,
    protocol::{Direction, TransportEvent, TransportService, UserProtocol},
    types::protocol::ProtocolName,
};

use bytes::{Buf, BytesMut};
use futures::{SinkExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio_util::codec::{Decoder, Encoder, Framed};

use std::collections::{HashMap, hash_map::Entry};

#[derive(Debug)]
struct FuzzCodec;

impl Decoder for FuzzCodec {
    type Item = BytesMut;
    type Error = litep2p::Error;

    /// We do not need to decode.
    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        Ok(None)
    }
}

impl Encoder<BytesMut> for FuzzCodec {
    type Error = std::io::Error;

    fn encode(&mut self, item: BytesMut, dst: &mut BytesMut) -> Result<(), Self::Error> {
        dst.extend(&item);
        Ok(())
    }
}

/// Commands sent to the protocol.
#[derive(Debug)]
enum FuzzProtocolCommand {
    /// Send `message` to `peer`.
    SendMessage { peer_id: PeerId, message: Vec<u8> },
}

#[derive(Debug)]
pub struct FuzzProtocol {
    name: &'static str,
    /// Channel for receiving commands from user.
    cmd_rx: Receiver<FuzzProtocolCommand>,

    /// Connected peers.
    peers: HashMap<PeerId, Option<Vec<u8>>>,

    /// Active inbound substreams.
    inbound: FuturesUnordered<BoxFuture<'static, (PeerId, Option<litep2p::Result<BytesMut>>)>>,

    /// Active outbound substreams.
    outbound: FuturesUnordered<BoxFuture<'static, litep2p::Result<()>>>,
}
#[async_trait::async_trait]
impl UserProtocol for FuzzProtocol {
    fn protocol(&self) -> ProtocolName {
        ProtocolName::from(self.name)
    }

    // Protocol code is set to `Unspecified` which means that `litep2p` won't provide
    // `Sink + Stream` for the protocol and instead only `AsyncWrite + AsyncRead` are provided.
    // User must implement their custom codec on top of `Substream` using, e.g.,
    // `tokio_codec::Framed` if they want to have message framing.
    fn codec(&self) -> ProtocolCodec {
        ProtocolCodec::Unspecified
    }

    /// Start running event loop for [`FuzzProtocol`].
    async fn run(mut self: Box<Self>, mut service: TransportService) -> litep2p::Result<()> {
        loop {
            tokio::select! {
                cmd = self.cmd_rx.recv() => match cmd {
                    Some(FuzzProtocolCommand::SendMessage { message, peer_id}) => {
                        let peer = peer_id;
                        match self.peers.entry(peer) {
                            // peer doens't exist so dial them and save the message
                            Entry::Vacant(entry) => match service.dial(&peer) {
                                Ok(()) => {
                                    entry.insert(Some(message));
                                }
                                Err(error) => {
                                    eprintln!("failed to dial {peer:?}: {error:?}");
                                }
                            }
                            // peer exists so open a new substream
                            Entry::Occupied(mut entry) => match service.open_substream(peer) {
                                Ok(_) => {
                                    entry.insert(Some(message));
                                }
                                Err(error) => {
                                    eprintln!("failed to open substream to {peer:?}: {error:?}");
                                }
                            }
                        }
                    }
                    None => return Err(litep2p::Error::EssentialTaskClosed),
                },
                event = service.next() => match event {
                    // connection established to peer
                    //
                    // check if the peer already exist in the protocol with a pending message
                    // and if yes, open substream to the peer.
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
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
                    // substream opened
                    //
                    // for inbound substreams, move the substream to `self.inbound` and poll them for messages
                    //
                    // for outbound substreams, move the substream to `self.outbound` and send the saved message to remote peer
                    Some(TransportEvent::SubstreamOpened { peer, substream, direction, .. }) => {
                        match direction {
                            Direction::Inbound => {
                                self.inbound.push(Box::pin(async move {
                                    (peer, Framed::new(substream, FuzzCodec).next().await)
                                }));
                            }
                            Direction::Outbound(_) => {
                                let message = self.peers.get_mut(&peer).expect("peer to exist").take().unwrap();

                                self.outbound.push(Box::pin(async move {
                                    let mut framed = Framed::new(substream, FuzzCodec);
                                    framed.send(BytesMut::from(&message[..])).await.map_err(From::from)
                                }));
                            }
                        }
                    }
                    // connection closed, remove all peer context
                    Some(TransportEvent::ConnectionClosed { peer }) => {
                        self.peers.remove(&peer);
                    }
                    None => return Err(litep2p::Error::EssentialTaskClosed),
                    _ => {},
                },
            }
        }
    }
}

impl FuzzProtocol {
    /// Create new [`FuzzProtocol`].
    pub fn new(name: &'static str) -> (Self, FuzzProtocolHandle) {
        let (cmd_tx, cmd_rx) = channel(64);

        (
            Self {
                name,
                cmd_rx,
                peers: HashMap::new(),
                inbound: FuturesUnordered::new(),
                outbound: FuturesUnordered::new(),
            },
            FuzzProtocolHandle { cmd_tx },
        )
    }
}

/// Handle for communicating with the protocol.
#[derive(Debug)]
pub struct FuzzProtocolHandle {
    cmd_tx: Sender<FuzzProtocolCommand>,
}

impl FuzzProtocolHandle {
    pub async fn send_message(&mut self, peer_id: PeerId, message: Vec<u8>) {
        let _ = self.cmd_tx.send(FuzzProtocolCommand::SendMessage { peer_id, message }).await;
    }
}
