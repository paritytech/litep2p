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
use futures::{Sink, Stream};

use std::{
	fmt::Debug,
	pin::Pin,
	task::{Context, Poll},
};

/// Trait which describes the behavior of a mock substream.
pub trait Substream:
	Debug + Stream<Item = crate::Result<BytesMut>> + Sink<Bytes, Error = Error> + Send + Unpin + 'static
{
}

/// Blanket implementation for [`Substream`].
impl<
		T: Debug
			+ Stream<Item = crate::Result<BytesMut>>
			+ Sink<Bytes, Error = Error>
			+ Send
			+ Unpin
			+ 'static,
	> Substream for T
{
}

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

/// Dummy substream which just implements `Stream + Sink` and returns `Poll::Pending`/`Ok(())`
#[derive(Debug)]
pub struct DummySubstream {}

impl DummySubstream {
	/// Create new [`DummySubstream`].
	#[cfg(test)]
	pub fn new() -> Self {
		Self {}
	}
}

impl Sink<bytes::Bytes> for DummySubstream {
	type Error = Error;

	fn poll_ready<'a>(self: Pin<&mut Self>, _cx: &mut Context<'a>) -> Poll<Result<(), Error>> {
		Poll::Pending
	}

	fn start_send(self: Pin<&mut Self>, _item: bytes::Bytes) -> Result<(), Error> {
		Ok(())
	}

	fn poll_flush<'a>(self: Pin<&mut Self>, _cx: &mut Context<'a>) -> Poll<Result<(), Error>> {
		Poll::Pending
	}

	fn poll_close<'a>(self: Pin<&mut Self>, _cx: &mut Context<'a>) -> Poll<Result<(), Error>> {
		Poll::Ready(Ok(()))
	}
}

impl Stream for DummySubstream {
	type Item = crate::Result<BytesMut>;

	fn poll_next<'a>(
		self: Pin<&mut Self>,
		_cx: &mut Context<'a>,
	) -> Poll<Option<crate::Result<BytesMut>>> {
		Poll::Pending
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use futures::SinkExt;

	#[tokio::test]
	async fn dummy_substream_sink() {
		let mut substream = DummySubstream::new();

		futures::future::poll_fn(|cx| match substream.poll_ready_unpin(cx) {
			Poll::Pending => Poll::Ready(()),
			_ => panic!("invalid event"),
		})
		.await;

		assert!(Pin::new(&mut substream).start_send(bytes::Bytes::new()).is_ok());

		futures::future::poll_fn(|cx| match substream.poll_flush_unpin(cx) {
			Poll::Pending => Poll::Ready(()),
			_ => panic!("invalid event"),
		})
		.await;

		futures::future::poll_fn(|cx| match substream.poll_close_unpin(cx) {
			Poll::Ready(Ok(())) => Poll::Ready(()),
			_ => panic!("invalid event"),
		})
		.await;
	}
}
