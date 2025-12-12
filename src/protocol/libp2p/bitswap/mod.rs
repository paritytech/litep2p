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

//! [`/ipfs/bitswap/1.2.0`](https://github.com/ipfs/specs/blob/main/BITSWAP.md) implementation.

use crate::{
    error::Error,
    protocol::{Direction, TransportEvent, TransportService},
    substream::Substream,
    types::{
        multihash::{Code, MultihashDigest},
        SubstreamId,
    },
    PeerId,
};

use cid::{Cid, Version};
use prost::Message;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::{StreamExt, StreamMap};

pub use config::Config;
pub use handle::{BitswapCommand, BitswapEvent, BitswapHandle, ResponseType};
pub use schema::bitswap::{wantlist::WantType, BlockPresenceType};
use std::{collections::HashMap, time::Duration};

mod config;
mod handle;

mod schema {
    pub(super) mod bitswap {
        include!(concat!(env!("OUT_DIR"), "/bitswap.rs"));
    }
}

/// Log target for the file.
const LOG_TARGET: &str = "litep2p::ipfs::bitswap";

/// Write timeout for outbound messages.
const WRITE_TIMEOUT: Duration = Duration::from_secs(15);

/// Bitswap metadata.
#[derive(Debug)]
struct Prefix {
    /// CID version.
    version: Version,

    /// CID codec.
    codec: u64,

    /// CID multihash type.
    multihash_type: u64,

    /// CID multihash length.
    multihash_len: u8,
}

impl Prefix {
    /// Convert the prefix to encoded bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut res = Vec::with_capacity(4 * 10);

        let mut buf = unsigned_varint::encode::u64_buffer();
        let version = unsigned_varint::encode::u64(self.version.into(), &mut buf);
        res.extend_from_slice(version);

        let mut buf = unsigned_varint::encode::u64_buffer();
        let codec = unsigned_varint::encode::u64(self.codec, &mut buf);
        res.extend_from_slice(codec);

        let mut buf = unsigned_varint::encode::u64_buffer();
        let multihash_type = unsigned_varint::encode::u64(self.multihash_type, &mut buf);
        res.extend_from_slice(multihash_type);

        let mut buf = unsigned_varint::encode::u64_buffer();
        let multihash_len = unsigned_varint::encode::u64(self.multihash_len as u64, &mut buf);
        res.extend_from_slice(multihash_len);
        res
    }

    /// Parse byte representation of prefix.
    pub fn from_bytes(prefix_bytes: &[u8]) -> Option<Prefix> {
        let (version, rest) = unsigned_varint::decode::u64(prefix_bytes).ok()?;
        let (codec, rest) = unsigned_varint::decode::u64(rest).ok()?;
        let (multihash_type, rest) = unsigned_varint::decode::u64(rest).ok()?;
        let (multihash_len, _rest) = unsigned_varint::decode::u64(rest).ok()?;

        let version = Version::try_from(version).ok()?;
        let multihash_len = u8::try_from(multihash_len).ok()?;

        Some(Prefix {
            version,
            codec,
            multihash_type,
            multihash_len,
        })
    }
}

/// Action to perform when substream is opened.
#[derive(Debug)]
enum SubstreamAction {
    /// Send a request.
    SendRequest(Vec<(Cid, WantType)>),
    /// Send a response.
    SendResponse(Vec<ResponseType>),
}

/// Bitswap protocol.
pub(crate) struct Bitswap {
    // Connection service.
    service: TransportService,

    /// TX channel for sending events to the user protocol.
    event_tx: Sender<BitswapEvent>,

    /// RX channel for receiving commands from `BitswapHandle`.
    cmd_rx: Receiver<BitswapCommand>,

    /// Pending outbound substreams.
    pending_outbound: HashMap<SubstreamId, SubstreamAction>,

    /// Inbound substreams.
    inbound: StreamMap<PeerId, Substream>,

    /// Peers waiting for dial.
    pending_dials: HashMap<PeerId, Vec<SubstreamAction>>,
}

impl Bitswap {
    /// Create new [`Bitswap`] protocol.
    pub(crate) fn new(service: TransportService, config: Config) -> Self {
        Self {
            service,
            cmd_rx: config.cmd_rx,
            event_tx: config.event_tx,
            pending_outbound: HashMap::new(),
            inbound: StreamMap::new(),
            pending_dials: HashMap::new(),
        }
    }

    /// Substream opened to remote peer.
    fn on_inbound_substream(&mut self, peer: PeerId, substream: Substream) {
        tracing::debug!(target: LOG_TARGET, ?peer, "handle inbound substream");

        if self.inbound.insert(peer, substream).is_some() {
            // Only one inbound substream per peer is allowed in order to constrain resources.
            tracing::debug!(
                target: LOG_TARGET,
                ?peer,
                "dropping inbound substream as remote opened a new one",
            );
        }
    }

    /// Message received from remote peer.
    async fn on_message_received(
        &mut self,
        peer: PeerId,
        message: bytes::BytesMut,
    ) -> Result<(), Error> {
        tracing::trace!(target: LOG_TARGET, ?peer, "handle inbound message");

        let message = schema::bitswap::Message::decode(message)?;

        // Check if this is a request (has wantlist with entries).
        if let Some(wantlist) = &message.wantlist {
            if !wantlist.entries.is_empty() {
                let cids = wantlist
                    .entries
                    .iter()
                    .filter_map(|entry| {
                        let cid = Cid::read_bytes(entry.block.as_slice()).ok()?;

                        let want_type = match entry.want_type {
                            0 => WantType::Block,
                            1 => WantType::Have,
                            _ => return None,
                        };

                        Some((cid, want_type))
                    })
                    .collect::<Vec<_>>();

                if !cids.is_empty() {
                    let _ = self.event_tx.send(BitswapEvent::Request { peer, cids }).await;
                }
            }
        }

        // Check if this is a response (has payload or block presences).
        if !message.payload.is_empty() || !message.block_presences.is_empty() {
            let mut responses = Vec::new();

            // Process payload (blocks).
            for block in message.payload {
                let Some(Prefix {
                    version,
                    codec,
                    multihash_type,
                    multihash_len: _,
                }) = Prefix::from_bytes(&block.prefix)
                else {
                    tracing::trace!(target: LOG_TARGET, ?peer, "invalid CID prefix received");
                    continue;
                };

                // Create multihash from the block data.
                let Ok(code) = Code::try_from(multihash_type) else {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        multihash_type,
                        "usupported multihash type",
                    );
                    continue;
                };

                let multihash = code.digest(&block.data);

                // We need to convert multihash to version supported by `cid` crate.
                let Ok(multihash) =
                    cid::multihash::Multihash::wrap(multihash.code(), multihash.digest())
                else {
                    tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        multihash_type,
                        "multihash size > 64 unsupported",
                    );
                    continue;
                };

                match Cid::new(version, codec, multihash) {
                    Ok(cid) => responses.push(ResponseType::Block {
                        cid,
                        block: block.data,
                    }),
                    Err(error) => tracing::trace!(
                        target: LOG_TARGET,
                        ?peer,
                        ?error,
                        "invalid CID received",
                    ),
                }
            }

            // Process block presences.
            for presence in message.block_presences {
                if let Ok(cid) = Cid::read_bytes(&presence.cid[..]) {
                    let presence_type = match presence.r#type {
                        0 => BlockPresenceType::Have,
                        1 => BlockPresenceType::DontHave,
                        _ => continue,
                    };

                    responses.push(ResponseType::Presence {
                        cid,
                        presence: presence_type,
                    });
                }
            }

            if !responses.is_empty() {
                let _ = self.event_tx.send(BitswapEvent::Response { peer, responses }).await;
            }
        }

        Ok(())
    }

    /// Handle opened outbound substream.
    async fn on_outbound_substream(
        &mut self,
        peer: PeerId,
        substream_id: SubstreamId,
        mut substream: Substream,
    ) {
        let Some(action) = self.pending_outbound.remove(&substream_id) else {
            tracing::warn!(target: LOG_TARGET, ?peer, ?substream_id, "pending outbound entry doesn't exist");
            return;
        };

        match action {
            SubstreamAction::SendRequest(cids) => {
                let request = schema::bitswap::Message {
                    wantlist: Some(schema::bitswap::Wantlist {
                        entries: cids
                            .into_iter()
                            .map(|(cid, want_type)| schema::bitswap::wantlist::Entry {
                                block: cid.to_bytes(),
                                priority: 1,
                                cancel: false,
                                want_type: want_type as i32,
                                send_dont_have: false,
                            })
                            .collect(),
                        full: false,
                    }),
                    ..Default::default()
                };

                let message = request.encode_to_vec().into();
                let _ = tokio::time::timeout(WRITE_TIMEOUT, substream.send_framed(message)).await;

                // Keep the substream open to receive the response.
                self.inbound.insert(peer, substream);
            }
            SubstreamAction::SendResponse(entries) => {
                let mut response = schema::bitswap::Message {
                    // `wantlist` field must always be present. This is what the official Kubo IPFS
                    // implementation does.
                    wantlist: Some(Default::default()),
                    ..Default::default()
                };

                for entry in entries {
                    match entry {
                        ResponseType::Block { cid, block } => {
                            let prefix = Prefix {
                                version: cid.version(),
                                codec: cid.codec(),
                                multihash_type: cid.hash().code(),
                                multihash_len: cid.hash().size(),
                            }
                            .to_bytes();

                            response.payload.push(schema::bitswap::Block {
                                prefix,
                                data: block,
                            });
                        }
                        ResponseType::Presence { cid, presence } => {
                            response.block_presences.push(schema::bitswap::BlockPresence {
                                cid: cid.to_bytes(),
                                r#type: presence as i32,
                            });
                        }
                    }
                }

                let message = response.encode_to_vec().into();
                let _ = tokio::time::timeout(WRITE_TIMEOUT, substream.send_framed(message)).await;

                substream.close().await;
            }
        }
    }

    /// Handle connection established event.
    fn on_connection_established(&mut self, peer: PeerId) {
        // If we have pending actions for this peer, open substreams.
        if let Some(actions) = self.pending_dials.remove(&peer) {
            for action in actions {
                match self.service.open_substream(peer) {
                    Ok(substream_id) => {
                        self.pending_outbound.insert(substream_id, action);
                    }
                    Err(error) => {
                        tracing::debug!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to open substream after connection established",
                        );
                    }
                }
            }
        }
    }

    /// Handle bitswap request.
    fn on_bitswap_request(&mut self, peer: PeerId, cids: Vec<(Cid, WantType)>) {
        match self.service.open_substream(peer) {
            Ok(substream_id) => {
                self.pending_outbound.insert(substream_id, SubstreamAction::SendRequest(cids));
            }
            Err(error) => {
                tracing::trace!(target: LOG_TARGET, ?peer, ?error, "failed to open substream, dialing peer");

                // Failed to open substream, try to dial the peer.
                match self.service.dial(&peer) {
                    Ok(()) => {
                        // Store the action to be performed when the peer is connected.
                        self.pending_dials
                            .entry(peer)
                            .or_default()
                            .push(SubstreamAction::SendRequest(cids));
                    }
                    Err(error) => {
                        tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to dial peer");
                    }
                }
            }
        }
    }

    /// Handle bitswap response.
    fn on_bitswap_response(&mut self, peer: PeerId, responses: Vec<ResponseType>) {
        match self.service.open_substream(peer) {
            Err(error) => {
                tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to open substream to peer")
            }
            Ok(substream_id) => {
                self.pending_outbound
                    .insert(substream_id, SubstreamAction::SendResponse(responses));
            }
        }
    }

    /// Start [`Bitswap`] event loop.
    pub async fn run(mut self) {
        tracing::debug!(target: LOG_TARGET, "starting bitswap event loop");

        loop {
            tokio::select! {
                event = self.service.next() => match event {
                    Some(TransportEvent::ConnectionEstablished { peer, .. }) => {
                        self.on_connection_established(peer);
                    }
                    Some(TransportEvent::SubstreamOpened {
                        peer,
                        substream,
                        direction,
                        ..
                    }) => match direction {
                        Direction::Inbound => self.on_inbound_substream(peer, substream),
                        Direction::Outbound(substream_id) =>
                            self.on_outbound_substream(peer, substream_id, substream).await,
                    },
                    None => return,
                    event => tracing::trace!(target: LOG_TARGET, ?event, "unhandled event"),
                },
                command = self.cmd_rx.recv() => match command {
                    Some(BitswapCommand::SendRequest { peer, cids }) => {
                        self.on_bitswap_request(peer, cids);
                    }
                    Some(BitswapCommand::SendResponse { peer, responses }) => {
                        self.on_bitswap_response(peer, responses);
                    }
                    None => return,
                },
                Some((peer, message)) = self.inbound.next(), if !self.inbound.is_empty() => {
                    match message {
                        Ok(message) => if let Err(e) = self.on_message_received(peer, message).await {
                            tracing::trace!(
                                target: LOG_TARGET,
                                ?peer,
                                ?e,
                                "error handling inbound message, dropping substream",
                            );
                            self.inbound.remove(&peer);
                        },
                        Err(e) => {
                            tracing::trace!(
                                target: LOG_TARGET,
                                ?peer,
                                ?e,
                                "inbound substream closed",
                            );
                            self.inbound.remove(&peer);
                        },
                    }
                }
            }
        }
    }
}
