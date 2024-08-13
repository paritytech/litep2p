// Copyright 2019 Parity Technologies (UK) Ltd.
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

#![allow(clippy::enum_variant_names)]

//! [`Litep2p`](`crate::Litep2p`) error types.

// TODO: clean up all these errors into something coherent
// TODO: move `NegotiationError` under `SubstreamError`

use crate::{
    protocol::Direction,
    transport::manager::limits::ConnectionLimitsError,
    types::{protocol::ProtocolName, ConnectionId, SubstreamId},
    PeerId,
};

use multiaddr::Multiaddr;
use multihash::{Multihash, MultihashGeneric};

use std::io::{self, ErrorKind};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Peer `{0}` does not exist")]
    PeerDoesntExist(PeerId),
    #[error("Peer `{0}` already exists")]
    PeerAlreadyExists(PeerId),
    #[error("Protocol `{0}` not supported")]
    ProtocolNotSupported(String),
    #[error("Address error: `{0}`")]
    AddressError(#[from] AddressError),
    #[error("Parse error: `{0}`")]
    ParseError(ParseError),
    #[error("I/O error: `{0}`")]
    IoError(ErrorKind),
    #[error("Negotiation error: `{0}`")]
    NegotiationError(#[from] NegotiationError),
    #[error("Substream error: `{0}`")]
    SubstreamError(#[from] SubstreamError),
    #[error("Substream error: `{0}`")]
    NotificationError(NotificationError),
    #[error("Essential task closed")]
    EssentialTaskClosed,
    #[error("Unknown error occurred")]
    Unknown,
    #[error("Cannot dial self: `{0}`")]
    CannotDialSelf(Multiaddr),
    #[error("Transport not supported")]
    TransportNotSupported(Multiaddr),
    #[error("Yamux error for substream `{0:?}`: `{1}`")]
    YamuxError(Direction, crate::yamux::ConnectionError),
    #[error("Operation not supported: `{0}`")]
    NotSupported(String),
    #[error("Other error occurred: `{0}`")]
    Other(String),
    #[error("Protocol already exists: `{0:?}`")]
    ProtocolAlreadyExists(ProtocolName),
    #[error("Operation timed out")]
    Timeout,
    #[error("Invalid state transition")]
    InvalidState,
    #[error("DNS address resolution failed")]
    DnsAddressResolutionFailed,
    #[error("Transport error: `{0}`")]
    TransportError(String),
    #[cfg(feature = "quic")]
    #[error("Failed to generate certificate: `{0}`")]
    CertificateGeneration(#[from] crate::crypto::tls::certificate::GenError),
    #[error("Invalid data")]
    InvalidData,
    #[error("Input rejected")]
    InputRejected,
    #[cfg(feature = "websocket")]
    #[error("WebSocket error: `{0}`")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::error::Error),
    #[error("Insufficient peers")]
    InsufficientPeers,
    #[error("Substream doens't exist")]
    SubstreamDoesntExist,
    #[cfg(feature = "webrtc")]
    #[error("`str0m` error: `{0}`")]
    WebRtc(#[from] str0m::RtcError),
    #[error("Remote peer disconnected")]
    Disconnected,
    #[error("Channel does not exist")]
    ChannelDoesntExist,
    #[error("Tried to dial self")]
    TriedToDialSelf,
    #[error("Litep2p is already connected to the peer")]
    AlreadyConnected,
    #[error("No addres available for `{0}`")]
    NoAddressAvailable(PeerId),
    #[error("Connection closed")]
    ConnectionClosed,
    #[cfg(feature = "quic")]
    #[error("Quinn error: `{0}`")]
    Quinn(quinn::ConnectionError),
    #[error("Invalid certificate")]
    InvalidCertificate,
    #[error("Peer ID mismatch: expected `{0}`, got `{1}`")]
    PeerIdMismatch(PeerId, PeerId),
    #[error("Channel is clogged")]
    ChannelClogged,
    #[error("Connection doesn't exist: `{0:?}`")]
    ConnectionDoesntExist(ConnectionId),
    #[error("Exceeded connection limits `{0:?}`")]
    ConnectionLimit(ConnectionLimitsError),
    #[error("Dial error: `{0}`")]
    DialError(#[from] DialError),
}

#[derive(Debug, thiserror::Error)]
pub enum AddressError {
    #[error("Invalid protocol")]
    InvalidProtocol,
    #[error("Invalid URL")]
    InvalidUrl,
    #[error("`PeerId` missing from the address")]
    PeerIdMissing,
    #[error("Address not available")]
    AddressNotAvailable,
    #[error("Invalid multihash: `{0:?}`")]
    InvalidMultihash(Multihash),
    #[error("Transport not supported")]
    TransportNotSupported(Multiaddr),
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Invalid multihash: `{0:?}`")]
    InvalidMultihash(Multihash),
    #[error("Failed to decode protobuf message: `{0:?}`")]
    ProstDecodeError(prost::DecodeError),
    #[error("Failed to encode protobuf message: `{0:?}`")]
    ProstEncodeError(prost::EncodeError),
    /// The protobuf message contains an unexpected key type.
    ///
    /// This error can happen when:
    ///  - The provided key type is not recognized.
    ///  - The provided key type is recognized but not supported.
    #[error("Unknown key type from protobuf message: `{0}`")]
    UnknownKeyType(i32),
    /// The public key bytes are invalid and cannot be parsed.
    ///
    /// This error can happen when:
    ///  - The received number of bytes is not equal to the expected number of bytes (32 bytes).
    ///  - The bytes are not a valid Ed25519 public key.
    #[error("Invalid public key")]
    InvalidPublicKey,
}

#[derive(Debug, thiserror::Error)]
pub enum SubstreamError {
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("yamux error: `{0}`")]
    YamuxError(crate::yamux::ConnectionError, Direction),
    #[error("Failed to read from substream, substream id `{0:?}`")]
    ReadFailure(Option<SubstreamId>),
    #[error("Failed to write to substream, substream id `{0:?}`")]
    WriteFailure(Option<SubstreamId>),
    #[error("Negotiation error: `{0:?}`")]
    NegotiationError(NegotiationError),
}

#[derive(Debug, thiserror::Error)]
pub enum NegotiationError {
    #[error("multistream-select error: `{0:?}`")]
    MultistreamSelectError(crate::multistream_select::NegotiationError),
    #[error("multistream-select error: `{0:?}`")]
    SnowError(snow::Error),
    #[error("Connection closed while negotiating")]
    ConnectionClosed,
    #[error("`PeerId` missing from Noise handshake")]
    PeerIdMissing,
    #[error("Operation timed out")]
    Timeout,
    #[error("Parse error: `{0}`")]
    ParseError(ParseError),
    #[error("I/O error: `{0}`")]
    IoError(ErrorKind),
    #[error("Expected a different noise state")]
    StateMissmatch,
    #[error("Peer ID mismatch: expected `{0}`, got `{1}`")]
    PeerIdMismatch(PeerId, PeerId),
    // TODO: Convert tokio_tungstenite::accept_async into `NegotiationError` for some cases (ie
    // ConnectionClosed).
    #[error("Other error occurred: `{0}`")]
    Other(String),
}

#[derive(Debug, thiserror::Error)]
pub enum NotificationError {
    #[error("Peer already exists")]
    PeerAlreadyExists,
    #[error("Peer is in invalid state")]
    InvalidState,
    #[error("Notifications clogged")]
    NotificationsClogged,
    #[error("Notification stream closed")]
    NotificationStreamClosed(PeerId),
}

#[derive(Debug, thiserror::Error)]
pub enum DialError {
    #[error("Dial timed out")]
    Timeout,
    #[error("Address error: `{0}`")]
    AddressError(#[from] AddressError),
    #[error("Dns lookup error for `{0}`")]
    DnsError(#[from] DnsError),
    #[error("Negotiation error: `{0}`")]
    NegotiationError(#[from] NegotiationError),
    #[error("I/O error: `{0}`")]
    IoError(ErrorKind),
    #[error("Tried to dial self")]
    TriedToDialSelf,
    #[error("Already connected to peer")]
    AlreadyConnected,
    #[error("Peer doens't have any known addresses")]
    NoAddressAvailable(PeerId),
    #[error("Peer ID mismatch: expected `{0}`, got `{1}`")]
    PeerIdMismatch(PeerId, PeerId),
    #[error("Exceeded connection limits `{0:?}`")]
    ConnectionLimit(ConnectionLimitsError),

    #[cfg(feature = "websocket")]
    #[error("WebSocket error: `{0}`")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::error::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("Dns failed to resolve url `{0}`")]
    ResolveError(String),
    #[error("DNS type is different from the provided IP address")]
    MismatchDnsVersion,
}

impl From<MultihashGeneric<64>> for Error {
    fn from(hash: MultihashGeneric<64>) -> Self {
        Error::ParseError(ParseError::InvalidMultihash(hash))
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::IoError(error.kind())
    }
}

impl From<io::Error> for DialError {
    fn from(error: io::Error) -> Self {
        DialError::IoError(error.kind())
    }
}

impl From<crate::multistream_select::NegotiationError> for Error {
    fn from(error: crate::multistream_select::NegotiationError) -> Error {
        Error::NegotiationError(NegotiationError::MultistreamSelectError(error))
    }
}

impl From<snow::Error> for Error {
    fn from(error: snow::Error) -> Self {
        Error::NegotiationError(NegotiationError::SnowError(error))
    }
}

impl<T> From<tokio::sync::mpsc::error::SendError<T>> for Error {
    fn from(_: tokio::sync::mpsc::error::SendError<T>) -> Self {
        Error::EssentialTaskClosed
    }
}

impl From<tokio::sync::oneshot::error::RecvError> for Error {
    fn from(_: tokio::sync::oneshot::error::RecvError) -> Self {
        Error::EssentialTaskClosed
    }
}

impl From<prost::DecodeError> for Error {
    fn from(error: prost::DecodeError) -> Self {
        Error::ParseError(ParseError::ProstDecodeError(error))
    }
}

impl From<prost::EncodeError> for Error {
    fn from(error: prost::EncodeError) -> Self {
        Error::ParseError(ParseError::ProstEncodeError(error))
    }
}

impl From<prost::DecodeError> for ParseError {
    fn from(error: prost::DecodeError) -> Self {
        ParseError::ProstDecodeError(error)
    }
}

impl From<prost::EncodeError> for ParseError {
    fn from(error: prost::EncodeError) -> Self {
        ParseError::ProstEncodeError(error)
    }
}

impl From<NegotiationError> for SubstreamError {
    fn from(error: NegotiationError) -> Self {
        SubstreamError::NegotiationError(error)
    }
}

impl From<prost::EncodeError> for NegotiationError {
    fn from(error: prost::EncodeError) -> Self {
        NegotiationError::ParseError(ParseError::ProstEncodeError(error))
    }
}

impl From<prost::DecodeError> for NegotiationError {
    fn from(error: prost::DecodeError) -> Self {
        NegotiationError::ParseError(ParseError::ProstDecodeError(error))
    }
}

impl From<snow::Error> for NegotiationError {
    fn from(error: snow::Error) -> Self {
        NegotiationError::SnowError(error)
    }
}

impl From<io::Error> for NegotiationError {
    fn from(error: io::Error) -> Self {
        NegotiationError::IoError(error.kind())
    }
}

impl From<ParseError> for NegotiationError {
    fn from(error: ParseError) -> Self {
        NegotiationError::ParseError(error)
    }
}

impl From<MultihashGeneric<64>> for AddressError {
    fn from(hash: MultihashGeneric<64>) -> Self {
        AddressError::InvalidMultihash(hash)
    }
}

#[cfg(feature = "quic")]
impl From<quinn::ConnectionError> for Error {
    fn from(error: quinn::ConnectionError) -> Self {
        match error {
            quinn::ConnectionError::TimedOut => Error::Timeout,
            error => Error::Quinn(error),
        }
    }
}

impl From<ConnectionLimitsError> for Error {
    fn from(error: ConnectionLimitsError) -> Self {
        Error::ConnectionLimit(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{channel, Sender};

    #[tokio::test]
    async fn try_from_errors() {
        tracing::trace!("{:?}", NotificationError::InvalidState);
        tracing::trace!("{:?}", DialError::AlreadyConnected);
        tracing::trace!(
            "{:?}",
            SubstreamError::YamuxError(crate::yamux::ConnectionError::Closed)
        );
        tracing::trace!("{:?}", AddressError::PeerIdMissing);
        tracing::trace!(
            "{:?}",
            ParseError::InvalidMultihash(Multihash::from(PeerId::random()))
        );

        let (tx, rx) = channel(1);
        drop(rx);

        async fn test(tx: Sender<()>) -> crate::Result<()> {
            tx.send(()).await.map_err(From::from)
        }

        match test(tx).await.unwrap_err() {
            Error::EssentialTaskClosed => {}
            _ => panic!("invalid error"),
        }
    }
}
