use crate::yamux::{
    connection::StreamCommand, tagged_stream::TaggedStream, ConnectionError, StreamId,
};
use futures::{channel::mpsc, stream::SelectAll, StreamExt};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

/// A [`Future`] that cleans up resources in case of an error.
#[must_use]
pub struct Cleanup {
    state: State,
    stream_receivers: SelectAll<TaggedStream<StreamId, mpsc::Receiver<StreamCommand>>>,
    error: Option<ConnectionError>,
}

impl Cleanup {
    pub(crate) fn new(
        stream_receivers: SelectAll<TaggedStream<StreamId, mpsc::Receiver<StreamCommand>>>,
        error: ConnectionError,
    ) -> Self {
        Self {
            state: State::ClosingStreamReceiver,
            stream_receivers,
            error: Some(error),
        }
    }
}

impl Future for Cleanup {
    type Output = ConnectionError;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match this.state {
                State::ClosingStreamReceiver => {
                    for stream in this.stream_receivers.iter_mut() {
                        stream.inner_mut().close();
                    }
                    this.state = State::DrainingStreamReceiver;
                }
                State::DrainingStreamReceiver => match this.stream_receivers.poll_next_unpin(cx) {
                    Poll::Ready(Some(cmd)) => {
                        drop(cmd);
                    }
                    Poll::Ready(None) | Poll::Pending =>
                        return Poll::Ready(
                            this.error.take().expect("to not be called after completion"),
                        ),
                },
            }
        }
    }
}

#[allow(clippy::enum_variant_names)]
enum State {
    ClosingStreamReceiver,
    DrainingStreamReceiver,
}
