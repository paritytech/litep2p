use crate::{
    error::Error,
    types::{ConnectionId, PeerId, ProtocolId, RequestId},
};

use multiaddr::{Multiaddr, Protocol};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
};

use std::{collections::HashMap, net::SocketAddr};

mod crypto;
mod error;
mod peer_id;
mod types;

/// Public result type used by the crate.
pub type Result<T> = std::result::Result<T, error::Error>;

/// Logging target for the file.
const LOG_TARGET: &str = "litep2p";

/// Default channel size.
const DEFAULT_CHANNEL_SIZE: usize = 64usize;

/// Interface defining the behavior expected from a transport.
///
/// Implemented by low-level transports such as TCP or QUIC.
#[async_trait::async_trait]
pub trait Transport {
    ///
    type Handle: Send;

    ///
    type EventStream: Send + Unpin;

    /// Create new [`Transport`] object and start listening on the specified address.
    async fn new(addres: Multiaddr) -> (Self::Handle, Self::EventStream);

    /// Open connection for remote peer at `address`.
    async fn open_connection(&self, address: Multiaddr) -> crate::Result<ConnectionId>;

    /// Open connection for remote peer at `address`.
    async fn close_connection(&self, connection: ConnectionId) -> crate::Result<ConnectionId>;
}

struct Tcp {}

struct TcpHandle {}

struct TcpEventStream {}

#[derive(Debug)]
enum ConnectionEvent {
    ConnectionAccepted(TcpStream),
}

#[async_trait::async_trait]
pub trait TransportService {
    /// Open connection for remote peer at `address`.
    async fn open_connection(&self, address: SocketAddr) -> crate::Result<TcpStream>;
}

#[async_trait::async_trait]
impl TransportService for TcpHandle {
    async fn open_connection(&self, address: SocketAddr) -> crate::Result<TcpStream> {
        TcpStream::connect(address)
            .await
            .map_err(|_| Error::Unknown)
    }
}

#[async_trait::async_trait]
impl Transport for Tcp {
    type Handle = TcpHandle;
    type EventStream = TcpEventStream;

    /// Create [`Transport`] implementation for [`Tcp`].
    async fn new(address: Multiaddr) -> (Self::Handle, Self::EventStream) {
        // TODO: this should not be parsed here
        let address = match address
            .iter()
            .next()
            .expect("network layer protocol to exist")
        {
            Protocol::Ip4(ip4_address) => {
                let port = match address
                    .iter()
                    .next()
                    .expect("transport layer protocol to exist")
                {
                    Protocol::Tcp(port) => port,
                    _ => todo!("other protocols not supported yet"),
                };

                SocketAddr::from((ip4_address, port))
            }
            Protocol::Ip6(ip6_address) => {
                let port = match address
                    .iter()
                    .next()
                    .expect("transport layer protocol to exist")
                {
                    Protocol::Tcp(port) => port,
                    _ => todo!("other protocols not supported yet"),
                };

                SocketAddr::from((ip6_address, port))
            }
            _ => todo!("other network layer protocols are not supported"),
        };

        match TcpListener::bind(address).await {
            Ok(listener) => {
                let (tx, _rx) = mpsc::channel(DEFAULT_CHANNEL_SIZE);

                tokio::spawn(async move {
                    while let Ok((stream, _)) = listener.accept().await {
                        tx.send(ConnectionEvent::ConnectionAccepted(stream))
                            .await
                            .expect("channel to stay open");
                    }
                });
            }
            Err(err) => {
                tracing::error!(
                    target: LOG_TARGET,
                    ?err,
                    ?address,
                    "failed to bind to address",
                );
            }
        }

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
        // TODO: create new connection for selected transport (TcpStream)?
        // TODO: make it noise-encrypted
        // TODO: use yamux
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
