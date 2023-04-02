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

//! Errors during identity key operations.

// TODO: clean up all these errors into something coherent
// TODO: move `NegotiationError` under `SubstreamError`

use crate::peer_id::PeerId;

use multiaddr::Protocol;
use multihash::{Multihash, MultihashGeneric};

use std::{
    error, fmt,
    io::{self, ErrorKind},
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Peer `{0}` does not exist")]
    PeerDoesntExist(PeerId),
    #[error("Protocol `{0}` not supported")]
    ProtocolNotSupported(String),
    #[error("Address error: `{0}`")]
    AddressError(AddressError),
    #[error("Parse error: `{0}`")]
    ParseError(ParseError),
    #[error("I/O error: `{0}`")]
    IoError(ErrorKind),
    #[error("Negotiation error: `{0}`")]
    NegotiationError(NegotiationError),
    #[error("Substream error")]
    SubstreamError(SubstreamError),
    #[error("Essential task closed")]
    EssentialTaskClosed,
    #[error("Unknown error occurred")]
    Unknown,
}

#[derive(Debug, thiserror::Error)]
pub enum AddressError {
    #[error("Invalid protocol")]
    InvalidProtocol,
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("Invalid multihash: `{0:?}`")]
    InvalidMultihash(Multihash),
    #[error("Failed to decode protobuf message: `{0:?}`")]
    ProstDecodeError(prost::DecodeError),
    #[error("Decoding error: `{0:?}`")]
    DecodingError(DecodingError),
}

#[derive(Debug, thiserror::Error)]
pub enum SubstreamError {
    #[error("Connection closed")]
    ConnectionClosed,
    #[error("yamux error: `{0}`")]
    YamuxError(yamux::ConnectionError),
}

#[derive(Debug, thiserror::Error)]
pub enum NegotiationError {
    #[error("multistream-select error: `{0:?}`")]
    MultistreamSelectError(multistream_select::NegotiationError),
    #[error("multistream-select error: `{0:?}`")]
    SnowError(snow::Error),
    #[error("Connection closed while negotiating")]
    ConnectionClosed,
    #[error("`PeerId` missing from Noise handshake")]
    PeerIdMissing,
}

// TODO: ???
pub enum RequestResponseError {
    Canceled,
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

impl From<multistream_select::NegotiationError> for Error {
    fn from(error: multistream_select::NegotiationError) -> Error {
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

impl From<prost::DecodeError> for Error {
    fn from(error: prost::DecodeError) -> Self {
        Error::ParseError(ParseError::ProstDecodeError(error))
    }
}

impl From<DecodingError> for Error {
    fn from(error: DecodingError) -> Self {
        Error::ParseError(ParseError::DecodingError(error))
    }
}

/// An error during decoding of key material.
#[derive(Debug)]
pub struct DecodingError {
    msg: String,
    source: Option<Box<dyn error::Error + Send + Sync>>,
}

impl DecodingError {
    pub(crate) fn missing_feature(feature_name: &'static str) -> Self {
        Self {
            msg: format!("cargo feature `{feature_name}` is not enabled"),
            source: None,
        }
    }

    pub(crate) fn failed_to_parse<E, S>(what: &'static str, source: S) -> Self
    where
        E: error::Error + Send + Sync + 'static,
        S: Into<Option<E>>,
    {
        Self {
            msg: format!("failed to parse {what}"),
            source: match source.into() {
                None => None,
                Some(e) => Some(Box::new(e)),
            },
        }
    }

    pub(crate) fn bad_protobuf(
        what: &'static str,
        source: impl error::Error + Send + Sync + 'static,
    ) -> Self {
        Self {
            msg: format!("failed to decode {what} from protobuf"),
            source: Some(Box::new(source)),
        }
    }

    pub(crate) fn unknown_key_type(key_type: i32) -> Self {
        Self {
            msg: format!("unknown key-type {key_type}"),
            source: None,
        }
    }

    pub(crate) fn decoding_unsupported(key_type: &'static str) -> Self {
        Self {
            msg: format!("decoding {key_type} key from Protobuf is unsupported"),
            source: None,
        }
    }
}

impl fmt::Display for DecodingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key decoding error: {}", self.msg)
    }
}

impl error::Error for DecodingError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.source.as_ref().map(|s| &**s as &dyn error::Error)
    }
}

/// An error during signing of a message.
#[derive(Debug)]
pub struct SigningError {
    msg: String,
    source: Option<Box<dyn error::Error + Send + Sync>>,
}

impl fmt::Display for SigningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Key signing error: {}", self.msg)
    }
}

impl error::Error for SigningError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        self.source.as_ref().map(|s| &**s as &dyn error::Error)
    }
}
