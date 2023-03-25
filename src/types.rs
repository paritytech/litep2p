use std::fmt::Display;

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
#[derive(Debug)]
pub enum ProtocolType {
    /// Notification protocol.
    Notification(ProtocolName),

    /// Request-response protocol.
    RequestResponse(ProtocolName),
}

// TODO: move this to `src/protocol/mod.rs`?
#[derive(Debug)]
pub enum ProtocolName {}
