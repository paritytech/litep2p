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

pub use crate::protocol::libp2p::{
    bitswap::BitswapEvent, identify::IdentifyEvent, kad::KademliaEvent, ping::PingEvent,
};

pub mod bitswap;
pub mod identify;
pub mod kad;
pub mod new_ping;
pub mod ping;

// TODO: remove this eventually
/// Events emitted by libp2p standard protocols.
#[derive(Debug)]
pub enum Libp2pProtocolEvent {
    /// Ping events.
    Bitswap(BitswapEvent),

    /// Identify events.
    Identify(IdentifyEvent),

    /// Kademlia events.
    Kademlia(KademliaEvent),

    /// Ping events.
    Ping(PingEvent),
}
