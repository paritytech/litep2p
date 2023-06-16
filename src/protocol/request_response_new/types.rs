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

use crate::types::protocol::ProtocolName;

use tokio::sync::mpsc::{channel, Receiver, Sender};

pub trait Encode {}

pub enum RequestReponseEvent {}

#[async_trait::async_trait]
pub trait RequestResponseService {
    type Request: Encode + Send;

    fn send_request(&mut self, peer: usize, request: Self::Request) -> usize;
    async fn next_event(&mut self) -> Option<RequestReponseEvent>;
}

pub enum InnerRequestResponseEvent {}
pub enum RequestResponseCommand {}

pub struct RequestResponseHandle<R: Encode + Send> {
    event_rx: Receiver<InnerRequestResponseEvent>,
    command_tx: Sender<RequestResponseCommand>,
    _marker: std::marker::PhantomData<R>,
}

impl<R: Encode + Send> RequestResponseHandle<R> {
    pub fn new(
        event_rx: Receiver<InnerRequestResponseEvent>,
        command_tx: Sender<RequestResponseCommand>,
    ) -> Self {
        Self {
            event_rx,
            command_tx,
            _marker: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl<R: Encode + Send> RequestResponseService for RequestResponseHandle<R> {
    type Request = R;

    fn send_request(&mut self, peer: usize, request: Self::Request) -> usize {
        todo!();
    }

    async fn next_event(&mut self) -> Option<RequestReponseEvent> {
        todo!();
    }
}

/// Request-response configuration.
#[derive(Debug)]
pub struct Config {
    protocol_name: ProtocolName,
    max_slots: usize,
    event_tx: Sender<InnerRequestResponseEvent>,
    command_rx: Receiver<RequestResponseCommand>,
}

impl Config {
    /// Create new [`Config`].
    pub fn new<R: Encode + Send>(
        protocol_name: ProtocolName,
        max_slots: usize,
    ) -> (Self, Box<dyn RequestResponseService<Request = R>>) {
        let (event_tx, event_rx) = channel(64);
        let (command_tx, command_rx) = channel(64);
        let handle = RequestResponseHandle::<R>::new(event_rx, command_tx);

        (
            Self {
                protocol_name,
                max_slots,
                event_tx,
                command_rx,
            },
            todo!(),
        )
    }

    /// Get protocol name.
    pub(crate) fn protocol_name(&self) -> &ProtocolName {
        &self.protocol_name
    }
}
