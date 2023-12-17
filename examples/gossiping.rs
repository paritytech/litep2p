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

#![allow(unused)]

//! This example demonstrates how application can implement block and transaction gossiping.
//!
//! Peers interested in both blocks and transactions initialize both notification protocols
//! and receive notifications over them. The block announce protocol only announces headers
//! and peers interested in the block bodies will request them over the request-response protocol.
//! The presence of the block announcement protocol assumes that the request-response protocol is
//! also available.
//!
//! TODO:

use litep2p::{
    config::Litep2pConfigBuilder,
    protocol::{
        notification::{
            Config as NotificationConfig, ConfigBuilder as NotificationConfigBuilder,
            NotificationHandle,
        },
        request_response::{
            Config as RequestResponseConfig, ConfigBuilder as RequestResponseConfigBuilder,
            RequestResponseHandle,
        },
    },
    transport::quic::config::TransportConfig as QuicTransportConfig,
    types::protocol::ProtocolName,
    Litep2p,
};

use futures::StreamExt;
use tokio::sync::{
    mpsc::{channel, Receiver, Sender},
    oneshot,
};

/// Dummy transaction.
#[derive(Hash)]
struct Transaction {
    tx: Vec<u8>,
}

/// Handle which allows communicating with [`TransactionProtocol`].
struct TransactionProtocolHandle {
    tx: Sender<Transaction>,
}

impl TransactionProtocolHandle {
    /// Create new [`TransactionProtocolHandle`].
    fn new() -> (Self, Receiver<Transaction>) {
        let (tx, rx) = channel(64);

        (Self { tx }, rx)
    }

    /// Announce transaction by sending it to the [`TransactionProtocol`] which will send
    /// it to all peers who don't have it yet.
    async fn announce_transaction(&self, tx: Transaction) {
        self.tx.send(tx).await;
    }
}

/// Transaction protocol.
struct TransactionProtocol {
    /// Notification handle used to send and receive notifications.
    tx_handle: NotificationHandle,
}

impl TransactionProtocol {
    fn new() -> (Self, NotificationConfig) {
        let (tx_config, tx_handle) = Self::init_tx_announce();

        (Self { tx_handle }, tx_config)
    }

    /// Initialize notification protocol for transactions.
    fn init_tx_announce() -> (NotificationConfig, NotificationHandle) {
        NotificationConfigBuilder::new(ProtocolName::from("/notif/tx/1"))
            .with_max_size(1024usize)
            .with_handshake(vec![1, 2, 3, 4])
            .build()
    }

    /// Start event loop for the transaction protocol.
    async fn run(&mut self) {
        todo!();
    }
}

/// Dummy block.
#[derive(Hash)]
struct Block {
    block: Vec<u8>,
}

/// Block protocol command.
enum BlockProtocolCommand {
    /// Announce block.
    AnnounceBlock(Block),

    /// Download block.
    DownloadBlock(u64, oneshot::Sender<Block>),
}

struct BlockProtocolHandle {
    tx: Sender<BlockProtocolCommand>,
}

impl BlockProtocolHandle {
    /// Create new [`BlockProtocolHandle`].
    fn new() -> (Self, Receiver<BlockProtocolCommand>) {
        let (tx, rx) = channel(64);

        (Self { tx }, rx)
    }

    /// Announce block on the network.
    async fn announce_block(&self, block: Block) {
        self.tx.send(BlockProtocolCommand::AnnounceBlock(block)).await;
    }

    /// Download block.
    async fn download_block(&self, hash: u64) -> Option<Block> {
        let (tx, rx) = oneshot::channel();

        self.tx.send(BlockProtocolCommand::DownloadBlock(hash, tx)).await;
        rx.await.ok()
    }
}

struct BlockProtocol {
    /// Notification handle used to send and receive notifications.
    block_announce_handle: NotificationHandle,

    /// Request-response handle for sending/receiving block requests.
    block_request_handle: RequestResponseHandle,
}

impl BlockProtocol {
    /// Create new [`SyncingEngine`].
    fn new() -> (Self, NotificationConfig, RequestResponseConfig) {
        let (block_announce_config, block_announce_handle) = Self::init_block_announce();
        let (block_request_config, block_request_handle) = Self::init_block_request();

        (
            Self {
                block_announce_handle,
                block_request_handle,
            },
            block_announce_config,
            block_request_config,
        )
    }

    /// Initialize notification protocol for block announcements
    fn init_block_announce() -> (NotificationConfig, NotificationHandle) {
        NotificationConfigBuilder::new(ProtocolName::from("/notif/block-announce/1"))
            .with_max_size(1024usize)
            .with_handshake(vec![1, 2, 3, 4])
            .build()
    }

    /// Initialize request-response protocol for block requests.
    fn init_block_request() -> (RequestResponseConfig, RequestResponseHandle) {
        RequestResponseConfigBuilder::new(ProtocolName::from("/sync/block/1"))
            .with_max_size(8 * 1024)
            .build()
    }

    /// Start event loop for [`SyncingEngine`].
    async fn run(mut self) {
        loop {
            tokio::select! {
                _ = self.block_announce_handle.next() => {}
                _ = self.block_request_handle.next() => {}
            }
        }
    }
}

/// Initialize peer with all protocols enabled.
async fn full_peer() -> (Litep2p, BlockProtocolHandle, TransactionProtocolHandle) {
    // create `BlockProtocol` and get configs for the protocols that it will use.
    let (mut block, block_announce_config, block_request_config) = BlockProtocol::new();
    let (mut tx, tx_announce_config) = TransactionProtocol::new();

    // build `Litep2pConfig`
    let config = Litep2pConfigBuilder::new()
        .with_quic(QuicTransportConfig {
            listen_addresses: vec!["/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()],
            ..Default::default()
        })
        .with_notification_protocol(block_announce_config)
        .with_notification_protocol(tx_announce_config)
        .with_request_response_protocol(block_request_config)
        .build();

    // create `Litep2p` object and start internal protocol handlers and the QUIC transport
    let mut litep2p = Litep2p::new(config).await.unwrap();

    // spawn `SyncingEngine` in the background
    tokio::spawn(block.run());
    // tokio::spawn(tx.run());

    todo!();
}

/// Initialize peer with block announcement/request-response protocols enabled.
async fn block_peer() -> (Litep2p, BlockProtocol) {
    todo!();
}

/// Initialize peer with transaction protocol enabled.
async fn tx_peer() -> (Litep2p, TransactionProtocol) {
    todo!();
}

#[tokio::main]
async fn main() {
    // create `BlockProtocol` and get configs for the protocols that it will use.
    let (engine, block_announce_config, block_request_config) = BlockProtocol::new();

    // build `Litep2pConfig`
    let config = Litep2pConfigBuilder::new()
        .with_quic(QuicTransportConfig {
            listen_addresses: vec!["/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap()],
            ..Default::default()
        })
        .with_notification_protocol(block_announce_config)
        .build();

    // create `Litep2p` object and start internal protocol handlers and the QUIC transport
    let mut litep2p = Litep2p::new(config).await.unwrap();

    // spawn `SyncingEngine` in the background
    tokio::spawn(engine.run());

    // poll `litep2p` to allow connection-related activity to make progress
    loop {
        match litep2p.next_event().await.unwrap() {
            _ => {}
        }
    }
}
