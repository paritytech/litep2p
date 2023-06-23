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

use bytes::BytesMut;
use futures::{Sink, Stream};

use std::{
    pin::Pin,
    task::{Context, Poll},
};

mockall::mock! {
    #[derive(Debug)]
    pub Substream {}

     impl Sink<bytes::Bytes> for Substream {
        type Error = Error;

        fn poll_ready<'a>(
            self: Pin<&mut Self>,
            cx: &mut Context<'a>
        ) -> Poll<Result<(), Error>>;

        fn start_send(self: Pin<&mut Self>, item: bytes::Bytes) -> Result<(), Error>;

        fn poll_flush<'a>(
            self: Pin<&mut Self>,
            cx: &mut Context<'a>
        ) -> Poll<Result<(), Error>>;

        fn poll_close<'a>(
            self: Pin<&mut Self>,
            cx: &mut Context<'a>
        ) -> Poll<Result<(), Error>>;
    }

    impl Stream for Substream {
        type Item = crate::Result<BytesMut>;

        fn poll_next<'a>(
            self: Pin<&mut Self>,
            cx: &mut Context<'a>
        ) -> Poll<Option<crate::Result<BytesMut>>>;
    }
}
