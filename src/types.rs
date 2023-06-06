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

use std::fmt::Display;

pub mod protocol;

/// Unique identifier for a peer.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PeerId(usize);

impl Display for PeerId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a connection.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ConnectionId(usize);

impl Display for ConnectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a request.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct RequestId(usize);

impl Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Unique identifier for a substream.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct SubstreamId(usize);

impl Display for SubstreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

// TODO: move this to `src/protocol/mod.rs`?
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProtocolId {
    /// Kademlia DHT.
    Kademlia,
}

// TODO: move this to `src/protocol/mod.rs`?
#[derive(Debug, Clone)]
pub enum ProtocolType {
    /// Notification protocol.
    Notification(ProtocolName),

    /// Request-response protocol.
    RequestResponse(ProtocolName),
}

// TODO: move this to `src/protocol/mod.rs`?
#[derive(Debug, Clone)]
pub enum ProtocolName {}
