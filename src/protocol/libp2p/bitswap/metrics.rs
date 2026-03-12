// Copyright 2025 litep2p developers
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

//! Bitswap protocol metrics.
//!
//! Provides atomic counters for bitswap operations, following the [`BandwidthSink`] pattern.
//! Consumers can read these counters and export them to their metrics system (e.g., Prometheus).

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

/// Inner bitswap metrics.
#[derive(Debug, Default)]
struct Inner {
    /// Number of incoming requests received.
    requests_received: AtomicUsize,
    /// Total CIDs requested across all incoming requests.
    cids_requested: AtomicUsize,
    /// Number of blocks sent in responses.
    blocks_sent: AtomicUsize,
    /// Total bytes of block data sent in responses.
    blocks_sent_bytes: AtomicUsize,
    /// Number of presence responses sent (Have + DontHave).
    presences_sent: AtomicUsize,
    /// Number of DontHave responses sent.
    dont_have_sent: AtomicUsize,
    /// Number of requests sent to remote peers.
    requests_sent: AtomicUsize,
    /// Number of responses received from remote peers.
    responses_received: AtomicUsize,
    /// Number of blocks received in responses.
    blocks_received: AtomicUsize,
    /// Total bytes of block data received in responses.
    blocks_received_bytes: AtomicUsize,
}

/// Bitswap protocol metrics.
///
/// Provides atomic counters for monitoring bitswap protocol activity.
/// Clone is cheap (Arc).
#[derive(Debug, Clone, Default)]
pub struct BitswapMetrics(Arc<Inner>);

impl BitswapMetrics {
    /// Create new [`BitswapMetrics`].
    pub(crate) fn new() -> Self {
        Self(Arc::new(Inner::default()))
    }

    /// Record an incoming request with the given number of CIDs.
    pub(crate) fn on_request_received(&self, cid_count: usize) {
        self.0.requests_received.fetch_add(1, Ordering::Relaxed);
        self.0.cids_requested.fetch_add(cid_count, Ordering::Relaxed);
    }

    /// Record blocks sent in a response.
    pub(crate) fn on_blocks_sent(&self, count: usize, bytes: usize) {
        self.0.blocks_sent.fetch_add(count, Ordering::Relaxed);
        self.0.blocks_sent_bytes.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Record presence responses sent.
    pub(crate) fn on_presences_sent(&self, count: usize, dont_have_count: usize) {
        self.0.presences_sent.fetch_add(count, Ordering::Relaxed);
        self.0.dont_have_sent.fetch_add(dont_have_count, Ordering::Relaxed);
    }

    /// Record a request sent to a remote peer.
    pub(crate) fn on_request_sent(&self) {
        self.0.requests_sent.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a response received from a remote peer.
    pub(crate) fn on_response_received(&self, block_count: usize, block_bytes: usize) {
        self.0.responses_received.fetch_add(1, Ordering::Relaxed);
        self.0.blocks_received.fetch_add(block_count, Ordering::Relaxed);
        self.0.blocks_received_bytes.fetch_add(block_bytes, Ordering::Relaxed);
    }

    /// Total incoming requests received.
    pub fn requests_received(&self) -> usize {
        self.0.requests_received.load(Ordering::Relaxed)
    }

    /// Total CIDs requested across all incoming requests.
    pub fn cids_requested(&self) -> usize {
        self.0.cids_requested.load(Ordering::Relaxed)
    }

    /// Total blocks sent in responses.
    pub fn blocks_sent(&self) -> usize {
        self.0.blocks_sent.load(Ordering::Relaxed)
    }

    /// Total bytes of block data sent in responses.
    pub fn blocks_sent_bytes(&self) -> usize {
        self.0.blocks_sent_bytes.load(Ordering::Relaxed)
    }

    /// Total presence responses sent (Have + DontHave).
    pub fn presences_sent(&self) -> usize {
        self.0.presences_sent.load(Ordering::Relaxed)
    }

    /// Total DontHave responses sent.
    pub fn dont_have_sent(&self) -> usize {
        self.0.dont_have_sent.load(Ordering::Relaxed)
    }

    /// Total requests sent to remote peers.
    pub fn requests_sent(&self) -> usize {
        self.0.requests_sent.load(Ordering::Relaxed)
    }

    /// Total responses received from remote peers.
    pub fn responses_received(&self) -> usize {
        self.0.responses_received.load(Ordering::Relaxed)
    }

    /// Total blocks received in responses.
    pub fn blocks_received(&self) -> usize {
        self.0.blocks_received.load(Ordering::Relaxed)
    }

    /// Total bytes of block data received in responses.
    pub fn blocks_received_bytes(&self) -> usize {
        self.0.blocks_received_bytes.load(Ordering::Relaxed)
    }
}
