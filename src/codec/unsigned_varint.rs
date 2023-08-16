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

use crate::error::Error;

use bytes::{Bytes, BytesMut};
use tokio_util::codec::{Decoder, Encoder};
use unsigned_varint::codec::UviBytes;

use std::fmt;

#[derive(Default)]
pub struct UnsignedVarint {
    codec: UviBytes<bytes::Bytes>,
}

impl fmt::Debug for UnsignedVarint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnsignedVarint").finish()
    }
}

impl UnsignedVarint {
    /// Create new [`UnsignedVarint`] codec.
    pub fn new() -> Self {
        Self {
            codec: UviBytes::<Bytes>::default(),
        }
    }

    /// Encode `payload` using `unsigned-varint`.
    // TODO: return `BytesMut`
    pub fn encode<T: Into<Bytes>>(payload: T) -> crate::Result<Vec<u8>> {
        let payload: Bytes = payload.into();

        assert!(payload.len() <= u32::MAX as usize);

        let mut bytes = BytesMut::with_capacity(payload.len() + 4);
        let mut codec = Self::new();
        codec.encode(payload.into(), &mut bytes)?;

        Ok(bytes.into())
    }

    /// Decode `payload` into `BytesMut`.
    pub fn decode(payload: &mut BytesMut) -> crate::Result<BytesMut> {
        Ok(UviBytes::<Bytes>::default()
            .decode(payload)?
            .ok_or(Error::InvalidData)?)
    }
}

impl Decoder for UnsignedVarint {
    type Item = BytesMut;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        self.codec.decode(src).map_err(From::from)
    }
}

impl Encoder<Bytes> for UnsignedVarint {
    type Error = Error;

    fn encode(&mut self, item: Bytes, dst: &mut bytes::BytesMut) -> Result<(), Self::Error> {
        self.codec.encode(item, dst).map_err(From::from)
    }
}
