// Copyright 2026 litep2p developers
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

//! Sockets for WebRTC.

use std::{
    io::{self, IoSliceMut},
    net::{IpAddr, SocketAddr},
    task::{ready, Context, Poll},
};

use quinn_udp::{RecvMeta, Transmit, UdpSockRef, UdpSocketState};
use tokio::{io::Interest, net::UdpSocket};

/// UDP socket paired with its `quinn-udp` state.
pub(crate) struct WebRtcSocket {
    /// Tokio UDP socket.
    socket: UdpSocket,
    /// `quinn-udp` socket state.
    state: UdpSocketState,
}

impl WebRtcSocket {
    /// Wrap a socket, setting up `IP_PKTINFO`/`IPV6_RECVPKTINFO` & GRO where available.
    pub(crate) fn new(socket: UdpSocket) -> io::Result<Self> {
        let state = UdpSocketState::new(UdpSockRef::from(&socket))?;
        Ok(Self { socket, state })
    }

    /// Poll a single read. The filled part of `buf` (`meta.len` bytes) may contain
    /// multiple GRO-coalesced datagrams (see [`RecvMeta::stride`]).
    pub(crate) fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<RecvMeta>> {
        loop {
            ready!(self.socket.poll_recv_ready(cx))?;

            let mut meta = RecvMeta::default();
            let mut iov = [IoSliceMut::new(&mut *buf)];
            match self.socket.try_io(Interest::READABLE, || {
                self.state.recv(
                    UdpSockRef::from(&self.socket),
                    &mut iov,
                    std::slice::from_mut(&mut meta),
                )
            }) {
                Ok(_) => return Poll::Ready(Ok(meta)),
                // Readiness was a false positive; re-poll to register the waker
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Poll::Ready(Err(e)),
            }
        }
    }

    /// Send `datagram` to `destination` from `src_ip`.
    pub(super) fn try_send_to(
        &self,
        datagram: &[u8],
        destination: SocketAddr,
        src_ip: IpAddr,
    ) -> io::Result<()> {
        let transmit = Transmit {
            destination,
            ecn: None,
            contents: datagram,
            segment_size: None,
            src_ip: Some(src_ip),
        };
        self.socket.try_io(Interest::WRITABLE, || {
            self.state.try_send(UdpSockRef::from(&self.socket), &transmit)
        })
    }
}
