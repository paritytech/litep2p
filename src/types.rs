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

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ProtocolId {
    /// Kademlia DHT.
    Kademlia,
}
