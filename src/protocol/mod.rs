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

//! Protocol-related defines.

use crate::{
    peer_id::PeerId,
    transport::{Connection, TransportEvent},
};

use tokio::sync::{mpsc, oneshot};

use std::fmt::Display;

pub mod libp2p;
pub mod notification;
pub mod request_response;

/// Commands sent by different protocols to `Litep2p`.
#[derive(Debug)]
pub enum TransportCommand {
    /// Open substream to remote peer.
    OpenSubstream {
        /// Protocol.
        protocol: String,

        /// Remote peer ID.
        peer: PeerId,
    },
}

#[derive(Debug, Clone)]
pub enum ProtocolName {
    /// Static protocol name.
    Static(&'static str),
}

impl Display for ProtocolName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self)
    }
}

impl From<&'static str> for ProtocolName {
    fn from(value: &'static str) -> Self {
        ProtocolName::Static(value)
    }
}

/// Libp2p protocol configuration.
#[derive(Debug)]
pub struct Libp2pProtocol {
    /// Protocol name.
    name: ProtocolName,
}

impl Libp2pProtocol {
    /// Create new [`Libp2pProtocol`].
    pub fn new(name: ProtocolName) -> Self {
        Self { name }
    }

    /// Get the name of the protocol.
    pub fn name(&self) -> &ProtocolName {
        &self.name
    }

    /// Get the name as `String`.
    pub fn to_string(&self) -> String {
        println!("convert {} to string", self.name);
        self.name.to_string()
    }
}

/// Notification protocol configuration.
#[derive(Debug)]
pub struct NotificationProtocol {
    /// Protocol name.
    name: ProtocolName,
}

impl NotificationProtocol {
    /// Create new [`NotificationProtocol`].
    pub fn new(name: ProtocolName) -> Self {
        Self { name }
    }

    /// Get the name of the protocol.
    pub fn name(&self) -> &ProtocolName {
        &self.name
    }

    /// Get the name as `String`.
    pub fn to_string(&self) -> String {
        self.name.to_string()
    }
}
