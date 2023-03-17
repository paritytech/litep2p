use crate::types::{ConnectionId, PeerId, ProtocolId, RequestId};

use multiaddr::Multiaddr;

use std::collections::HashMap;

pub mod error;
pub mod types;

/// Public result type used by the crate.
pub type Result<T> = std::result::Result<T, error::Error>;

/// Interface defining the behavior expected from a transport.
///
/// Implemented by low-level transports such as TCP or QUIC.
#[async_trait::async_trait]
pub trait Transport {
    ///
    type Handle: Send;

    ///
    type EventStream: Send + Unpin;

    /// Create new [`Transport`] object.
    fn new() -> (Self::Handle, Self::EventStream);

    /// Open connection for remote peer at `address`.
    async fn open_connection(&self, address: Multiaddr) -> crate::Result<ConnectionId>;

    /// Open connection for remote peer at `address`.
    async fn close_connection(&self, connection: ConnectionId) -> crate::Result<ConnectionId>;

    /// Send `data` to remote peer.
    fn send(&mut self, data: Vec<u8>) -> crate::Result<()>;
}

struct Tcp {}

struct TcpHandle {}

struct TcpEventStream {}

#[async_trait::async_trait]
impl Transport for Tcp {
    type Handle = TcpHandle;
    type EventStream = TcpEventStream;

    /// Create [`Transport`] implementation for [`Tcp`].
    fn new() -> (Self::Handle, Self::EventStream) {
        todo!();
    }

    /// Open connection for remote peer at `address`.
    async fn open_connection(&self, _address: Multiaddr) -> crate::Result<ConnectionId> {
        todo!();
    }

    /// Open connection for remote peer at `address`.
    async fn close_connection(&self, _connection: ConnectionId) -> crate::Result<ConnectionId> {
        todo!();
    }

    /// Send `data` to remote peer.
    fn send(&mut self, _data: Vec<u8>) -> crate::Result<()> {
        todo!();
    }
}

struct ConnectionContext<T: Transport> {
    _connection: ConnectionId,
    _handle: T::Handle,
}

struct RequestContext {
    _peer: PeerId,
}

pub struct Litep2p<T: Transport> {
    _connections: HashMap<ConnectionId, ConnectionContext<T>>,
    _pending_requests: HashMap<RequestId, RequestContext>,
}

// TODO: how to support multiple transports?
impl<T: Transport> Litep2p<T> {
    /// Create new [`Litep2p`] object.
    pub fn new() -> Self {
        Self {
            _connections: HashMap::new(),
            _pending_requests: HashMap::new(),
        }
    }

    /// Open connection to remote peer at `address`.
    // TODO: this can't block, make it async?
    // TODO: how to implement rate-limiting for a connection
    pub async fn open_connection(&mut self, _address: Multiaddr) -> crate::Result<PeerId> {
        todo!();
    }

    /// Close connection to remote `peer`.
    pub fn close_connection(&mut self, _peer: PeerId) -> crate::Result<()> {
        todo!();
    }

    /// Open substream with `peer` for `protocol`.
    // TODO: return (handle, sink) pair that allows sending and receiving notifications from the substream?
    // TODO: how to implement rate-limiting for substream?
    pub fn open_substream(&mut self, _peer: PeerId, _protocol: ProtocolId) -> crate::Result<()> {
        todo!()
    }

    /// Send request to `peer` over `protocol`
    // TODO: timeouts
    // TODO: rate-limiting?
    pub fn send_request(
        &mut self,
        _peer: PeerId,
        _protocol: ProtocolId,
        _request: Vec<u8>,
    ) -> crate::Result<()> {
        todo!();
    }

    /// Send response to `request`.
    pub fn send_response(&mut self, _request: RequestId, _response: Vec<u8>) -> crate::Result<()> {
        todo!();
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn two_transport_supported() {
        assert_eq!(1 + 1, 2);
    }
}
