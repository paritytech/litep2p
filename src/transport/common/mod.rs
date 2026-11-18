// Copyright 2024 litep2p developers
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

//! Shared transport protocol implementation

use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::{
    sync::OwnedSemaphorePermit,
    time::{error::Elapsed, timeout_at, Instant as TokioInstant},
};

pub mod listener;

pub(crate) fn opening_deadline(now: Instant, timeout: Duration) -> Instant {
    // Match Tokio's far-future fallback on overflow.
    now.checked_add(timeout)
        .unwrap_or_else(|| now + Duration::from_secs(86400 * 365 * 30))
}

/// Holds inbound capacity and a deadline in the transport's clock.
pub(crate) struct InboundOpening<T> {
    _permit: OwnedSemaphorePermit,
    pub(crate) deadline: T,
}

impl<T> InboundOpening<T> {
    pub(crate) fn new(permit: OwnedSemaphorePermit, deadline: T) -> Self {
        Self {
            _permit: permit,
            deadline,
        }
    }
}

impl InboundOpening<TokioInstant> {
    pub(crate) fn with_timeout(permit: OwnedSemaphorePermit, timeout: Duration) -> Self {
        Self::new(
            permit,
            opening_deadline(TokioInstant::now().into_std(), timeout).into(),
        )
    }

    pub(crate) async fn negotiate<F: Future>(self, handshake: F) -> Result<F::Output, Elapsed> {
        timeout_at(self.deadline, handshake).await
    }
}

#[test]
fn opening_deadline_handles_extreme_timeout() {
    let now = Instant::now();
    assert!(opening_deadline(now, Duration::MAX) > now);
}

/// Connected loopback TCP pair: `(client, server, client address)`.
#[cfg(test)]
pub(crate) async fn inbound_socket() -> (
    tokio::net::TcpStream,
    tokio::net::TcpStream,
    std::net::SocketAddr,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (client, server) = tokio::join!(
        tokio::net::TcpStream::connect(listener.local_addr().unwrap()),
        listener.accept()
    );
    let (server, address) = server.unwrap();
    (client.unwrap(), server, address)
}
