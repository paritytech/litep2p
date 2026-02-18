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
    error::{Error, ImmediateDialError},
    protocol::{Direction, TransportEvent, TransportService},
    substream::Substream,
    types::{
        multihash::{Code, MultihashDigest},
        SubstreamId,
    },
    PeerId,
};

use bytes::Bytes;
use cid::{Cid, Version};
use prost::Message;
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_stream::{StreamExt, StreamMap};

pub use config::Config;
pub use handle::{BitswapCommand, BitswapEvent, BitswapHandle, ResponseType};
pub use schema::bitswap::{wantlist::WantType, BlockPresenceType};
use std::{
    collections::{hash_map::Entry, vec_deque::Drain, HashMap, HashSet, VecDeque},
    time::Duration,
};

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
        let (multihash_len, rest) = unsigned_varint::decode::u64(rest).ok()?;
        if !rest.is_empty() {
            return None;
        }

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

    /// Pending outbound actions.
    pending_outbound: HashMap<PeerId, Vec<SubstreamAction>>,

    /// Inbound substreams.
    inbound: StreamMap<PeerId, Substream>,

    /// Outbound substreams.
    outbound: HashMap<PeerId, Substream>,

    /// Peers waiting for dial.
    pending_dials: HashSet<PeerId>,
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
            outbound: HashMap::new(),
            pending_dials: HashSet::new(),
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
        let Some(actions) = self.pending_outbound.remove(&peer) else {
            tracing::warn!(target: LOG_TARGET, ?peer, ?substream_id, "pending outbound entry doesn't exist");
            return;
        };

        tracing::trace!(target: LOG_TARGET, ?peer, "handle outbound substream");

        for action in actions {
            match action {
                SubstreamAction::SendRequest(cids) => {
                    if let Err(error) = send_request(&mut substream, cids).await {
                        // Drop the substream and all actions in case of sending error.
                        tracing::debug!(target: LOG_TARGET, ?peer, ?error, "bitswap request failed");
                        return;
                    }
                }
                SubstreamAction::SendResponse(entries) => {
                    if let Err(error) = send_response(&mut substream, entries).await {
                        // Drop the substream and all actions in case of sending error.
                        tracing::debug!(target: LOG_TARGET, ?peer, ?error, "bitswap response failed");
                        return;
                    }
                }
            }
        }

        self.outbound.insert(peer, substream);
    }

    /// Handle connection established event.
    fn on_connection_established(&mut self, peer: PeerId) {
        // If we have pending actions for this peer, open a substream.
        if self.pending_dials.remove(&peer) {
            tracing::trace!(
                target: LOG_TARGET,
                ?peer,
                "open substream after connection established",
            );

            if let Err(error) = self.service.open_substream(peer) {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    ?error,
                    "failed to open substream after connection established",
                );
                // Drop all pending actions; they are not going to be handled anyway, and we need
                // the entry to be empty to properly open subsequent substreams.
                self.pending_outbound.remove(&peer);
            }
        }
    }

    /// Open substream or dial a peer.
    fn open_substream_or_dial(&mut self, peer: PeerId) {
        tracing::trace!(target: LOG_TARGET, ?peer, "open substream");

        if let Err(error) = self.service.open_substream(peer) {
            tracing::trace!(
                target: LOG_TARGET,
                ?peer,
                ?error,
                "failed to open substream, dialing peer",
            );

            // Failed to open substream, try to dial the peer.
            match self.service.dial(&peer) {
                Ok(()) => {
                    // Store the peer to open a substream once it is connected.
                    self.pending_dials.insert(peer);
                }
                Err(ImmediateDialError::AlreadyConnected) => {
                    // By the time we tried to dial peer, it got connected.
                    if let Err(error) = self.service.open_substream(peer) {
                        tracing::trace!(
                            target: LOG_TARGET,
                            ?peer,
                            ?error,
                            "failed to open substream for a second time",
                        );
                    }
                }
                Err(error) => {
                    tracing::debug!(target: LOG_TARGET, ?peer, ?error, "failed to dial peer");
                }
            }
        }
    }

    /// Handle bitswap request.
    async fn on_bitswap_request(&mut self, peer: PeerId, cids: Vec<(Cid, WantType)>) {
        // Try to send request over existing substream first.
        if let Entry::Occupied(mut entry) = self.outbound.entry(peer) {
            if send_request(entry.get_mut(), cids.clone()).await.is_ok() {
                return;
            } else {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    "failed to send request over existing substream",
                );
                entry.remove();
            }
        }

        // Store pending actions for once the substream is opened.
        let pending_actions = self.pending_outbound.entry(peer).or_default();
        // If we inserted the default empty entry above, this means no pending substream
        // was requested by previous calls to `on_bitswap_request`. We will request a substream
        // in this case below.
        let no_substream_pending = pending_actions.is_empty();

        pending_actions.push(SubstreamAction::SendRequest(cids));

        if no_substream_pending {
            self.open_substream_or_dial(peer);
        }
    }

    /// Handle bitswap response.
    async fn on_bitswap_response(&mut self, peer: PeerId, responses: Vec<ResponseType>) {
        // Try to send response over existing substream first.
        if let Entry::Occupied(mut entry) = self.outbound.entry(peer) {
            if send_response(entry.get_mut(), responses.clone()).await.is_ok() {
                return;
            } else {
                tracing::debug!(
                    target: LOG_TARGET,
                    ?peer,
                    "failed to send response over existing substream",
                );
                entry.remove();
            }
        }

        // Store pending actions for later and open substream if not requested already.
        let pending_actions = self.pending_outbound.entry(peer).or_default();
        let no_pending_substream = pending_actions.is_empty();
        pending_actions.push(SubstreamAction::SendResponse(responses));

        if no_pending_substream {
            self.open_substream_or_dial(peer);
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
                        self.on_bitswap_request(peer, cids).await;
                    }
                    Some(BitswapCommand::SendResponse { peer, responses }) => {
                        self.on_bitswap_response(peer, responses).await;
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

async fn send_request(substream: &mut Substream, cids: Vec<(Cid, WantType)>) -> Result<(), Error> {
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
    match tokio::time::timeout(WRITE_TIMEOUT, substream.send_framed(message)).await {
        Err(_) => Err(Error::Timeout),
        Ok(Err(e)) => Err(Error::SubstreamError(e)),
        Ok(Ok(())) => Ok(()),
    }
}

async fn send_response(substream: &mut Substream, entries: Vec<ResponseType>) -> Result<(), Error> {
    // Send presences in a separate message to not deal with it when batching blocks below.
    if let Some((message, cid_count)) =
        presences_message(entries.iter().filter_map(|entry| match entry {
            ResponseType::Presence { cid, presence } => Some((*cid, *presence)),
            ResponseType::Block { .. } => None,
        }))
    {
        if message.len() <= config::MAX_MESSAGE_SIZE {
            tracing::trace!(
                target: LOG_TARGET,
                cid_count,
                "sending Bitswap presence message",
            );
            match tokio::time::timeout(WRITE_TIMEOUT, substream.send_framed(message)).await {
                Err(_) => return Err(Error::Timeout),
                Ok(Err(e)) => return Err(Error::SubstreamError(e)),
                Ok(Ok(())) => {}
            }
        } else {
            // This should never happen in practice, but log a warning if the presence message
            // exceeded [`config::MAX_MESSAGE_SIZE`].
            tracing::warn!(
                target: LOG_TARGET,
                size = message.len(),
                max_size = config::MAX_MESSAGE_SIZE,
                "outgoing Bitswap presence message exceeded max size",
            );
        }
    }

    // Send blocks in batches of up to [`config::MAX_BATCH_SIZE`] bytes.
    let mut blocks = entries
        .into_iter()
        .filter_map(|entry| match entry {
            ResponseType::Block { cid, block } => Some((cid, block)),
            ResponseType::Presence { .. } => None,
        })
        .collect::<VecDeque<_>>();

    while let Some(batch) = extract_next_batch(&mut blocks, config::MAX_BATCH_SIZE) {
        if let Some((message, block_count)) = blocks_message(batch) {
            if message.len() <= config::MAX_MESSAGE_SIZE {
                tracing::trace!(
                    target: LOG_TARGET,
                    block_count,
                    "sending Bitswap blocks message",
                );
                match tokio::time::timeout(WRITE_TIMEOUT, substream.send_framed(message)).await {
                    Err(_) => return Err(Error::Timeout),
                    Ok(Err(e)) => return Err(Error::SubstreamError(e)),
                    Ok(Ok(())) => {}
                }
            } else {
                // This should never happen in practice, but log a warning if the blocks message
                // exceeded [`config::MAX_MESSAGE_SIZE`].
                tracing::warn!(
                    target: LOG_TARGET,
                    size = message.len(),
                    max_size = config::MAX_MESSAGE_SIZE,
                    "outgoing Bitswap blocks message exceeded max size",
                );
            }
        }
    }

    Ok(())
}

fn presences_message(
    presences: impl IntoIterator<Item = (Cid, BlockPresenceType)>,
) -> Option<(Bytes, usize)> {
    let message = schema::bitswap::Message {
        // Set wantlist to not cause null pointer dereference in older versions of Kubo.
        wantlist: Some(Default::default()),
        block_presences: presences
            .into_iter()
            .map(|(cid, presence)| schema::bitswap::BlockPresence {
                cid: cid.to_bytes(),
                r#type: presence as i32,
            })
            .collect(),
        ..Default::default()
    };

    let count = message.block_presences.len();

    (count > 0).then(|| (message.encode_to_vec().into(), count))
}

fn blocks_message(blocks: impl IntoIterator<Item = (Cid, Vec<u8>)>) -> Option<(Bytes, usize)> {
    let message = schema::bitswap::Message {
        // Set wantlist to not cause null pointer dereference in older versions of Kubo.
        wantlist: Some(Default::default()),
        payload: blocks
            .into_iter()
            .map(|(cid, block)| {
                let prefix = Prefix {
                    version: cid.version(),
                    codec: cid.codec(),
                    multihash_type: cid.hash().code(),
                    multihash_len: cid.hash().size(),
                }
                .to_bytes();

                schema::bitswap::Block {
                    prefix,
                    data: block,
                }
            })
            .collect(),
        ..Default::default()
    };

    let count = message.payload.len();

    (count > 0).then(|| (message.encode_to_vec().into(), count))
}

/// Extract a batch of blocks of no more than `max_size` from `blocks`.
/// Returns `None` if no more blocks are left.
fn extract_next_batch<'a>(
    blocks: &'a mut VecDeque<(Cid, Vec<u8>)>,
    max_batch_size: usize,
) -> Option<Drain<'a, (Cid, Vec<u8>)>> {
    // Get rid of oversized blocks to not stall the processing by not being able to queue them.
    loop {
        if let Some(block) = blocks.front() {
            if block.1.len() > max_batch_size {
                tracing::warn!(
                    target: LOG_TARGET,
                    cid = block.0.to_string(),
                    size = block.1.len(),
                    max_batch_size,
                    "outgoing Bitswap block exceeded max batch size",
                );
                blocks.pop_front();
            } else {
                break;
            }
        } else {
            return None;
        }
    }

    // Determine how many blocks we can batch. Note that we can always batch at least one
    // block due to check above.
    let mut total_size = 0;
    let mut block_count = 0;

    for b in blocks.iter() {
        let next_block_size = b.1.len();
        if total_size + next_block_size > max_batch_size {
            break;
        }
        total_size += next_block_size;
        block_count += 1;
    }

    Some(blocks.drain(..block_count))
}

#[cfg(test)]
mod tests {
    use cid::multihash::Multihash;

    use super::*;

    fn cid(block: &[u8]) -> Cid {
        let codec = 0x55;
        let multihash = Code::Sha2_256.digest(block);
        let multihash =
            Multihash::wrap(multihash.code(), multihash.digest()).expect("to be valid multihash");

        Cid::new_v1(codec, multihash)
    }

    #[test]
    fn extract_next_batch_fits_max_size() {
        let max_size = 100;

        let block1 = vec![0x01; 10];
        let block2 = vec![0x02; 10];
        let block3 = vec![0x03; 10];

        let blocks = vec![
            (cid(&block1), block1),
            (cid(&block2), block2),
            (cid(&block3), block3),
        ];
        let mut blocks_deque = blocks.iter().cloned().collect::<VecDeque<_>>();

        let batch = extract_next_batch(&mut blocks_deque, max_size).unwrap();
        assert_eq!(batch.collect::<Vec<_>>(), blocks);

        assert!(extract_next_batch(&mut blocks_deque, max_size).is_none());
    }

    #[test]
    fn extract_next_batch_chunking_exact() {
        let max_size = 20;

        let block1 = vec![0x01; 10];
        let block2 = vec![0x02; 10];
        let block3 = vec![0x03; 10];

        let blocks = vec![
            (cid(&block1), block1.clone()),
            (cid(&block2), block2.clone()),
            (cid(&block3), block3.clone()),
        ];
        let chunk1 = vec![
            (cid(&block1), block1.clone()),
            (cid(&block2), block2.clone()),
        ];
        let chunk2 = vec![(cid(&block3), block3.clone())];
        let mut blocks_deque = blocks.iter().cloned().collect::<VecDeque<_>>();

        let batch = extract_next_batch(&mut blocks_deque, max_size).unwrap();
        assert_eq!(batch.collect::<Vec<_>>(), chunk1);

        let batch = extract_next_batch(&mut blocks_deque, max_size).unwrap();
        assert_eq!(batch.collect::<Vec<_>>(), chunk2);

        assert!(extract_next_batch(&mut blocks_deque, max_size).is_none());
    }

    #[test]
    fn extract_next_batch_chunking_less_than() {
        let max_size = 20;

        let block1 = vec![0x01; 10];
        let block2 = vec![0x02; 9];
        let block3 = vec![0x03; 10];

        let blocks = vec![
            (cid(&block1), block1.clone()),
            (cid(&block2), block2.clone()),
            (cid(&block3), block3.clone()),
        ];
        let chunk1 = vec![
            (cid(&block1), block1.clone()),
            (cid(&block2), block2.clone()),
        ];
        let chunk2 = vec![(cid(&block3), block3.clone())];
        let mut blocks_deque = blocks.iter().cloned().collect::<VecDeque<_>>();

        let batch = extract_next_batch(&mut blocks_deque, max_size).unwrap();
        assert_eq!(batch.collect::<Vec<_>>(), chunk1);

        let batch = extract_next_batch(&mut blocks_deque, max_size).unwrap();
        assert_eq!(batch.collect::<Vec<_>>(), chunk2);

        assert!(extract_next_batch(&mut blocks_deque, max_size).is_none());
    }

    #[test]
    fn extract_next_batch_oversized_blocks_discarded() {
        let max_size = 20;

        let block1 = vec![0x01; 10];
        let block2 = vec![0x02; 101];
        let block3 = vec![0x03; 10];

        let blocks = vec![
            (cid(&block1), block1.clone()),
            (cid(&block2), block2.clone()),
            (cid(&block3), block3.clone()),
        ];
        let chunk1 = vec![(cid(&block1), block1.clone())];
        let chunk2 = vec![(cid(&block3), block3.clone())];
        let mut blocks_deque = blocks.iter().cloned().collect::<VecDeque<_>>();

        let batch = extract_next_batch(&mut blocks_deque, max_size).unwrap();
        assert_eq!(batch.collect::<Vec<_>>(), chunk1);

        let batch = extract_next_batch(&mut blocks_deque, max_size).unwrap();
        assert_eq!(batch.collect::<Vec<_>>(), chunk2);

        assert!(extract_next_batch(&mut blocks_deque, max_size).is_none());
    }
}
