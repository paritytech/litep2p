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

use bytes::Bytes;
use tokio_util::codec::{Decoder, Encoder, Framed};

use std::sync::Arc;

pub mod identity;
pub mod unsigned_varint;

// TODO: documentation
pub trait Codec: Encoder<Bytes, Error = Error> + Decoder<Item = Bytes, Error = Error> {}

impl<T: Encoder<Bytes, Error = Error> + Decoder<Item = Bytes, Error = Error>> Codec for T {}

#[derive(Debug, Clone)]
pub enum ProtocolCodec {
    /// Identity codec where the argument denotes the payload size.
    Identity(usize),

    /// Unsigned varint.
    UnsignedVarint,
}
