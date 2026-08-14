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

/// Upper bound of a GRO segment size. Segments are bounded by the path MTU;
/// assume a conventional Ethernet MTU.
const MAX_GRO_SEGMENT_SIZE: usize = 1500;

/// Upper bound of a UDP payload size.
pub(crate) const MAX_DATAGRAM_SIZE: usize = 64 * 1024;

/// Local packet address aware UDP socket with GRO support.
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

    /// Buffer size [`Self::poll_recv`] needs to never truncate a read: enough for the largest
    /// GRO list the kernel can coalesce, and for a maximum-size datagram when GRO is off.
    pub(crate) fn max_read_size(&self) -> usize {
        (self.state.gro_segments() * MAX_GRO_SEGMENT_SIZE).max(MAX_DATAGRAM_SIZE)
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
    pub(crate) fn try_send_to(
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

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::poll_fn;
    use std::net::Ipv4Addr;

    async fn webrtc_socket() -> WebRtcSocket {
        WebRtcSocket::new(UdpSocket::bind("0.0.0.0:0").await.expect("bind socket"))
            .expect("wrap socket")
    }

    #[tokio::test]
    async fn wildcard_recv_reports_destination_address() {
        let receiver = webrtc_socket().await;
        let port = receiver.socket.local_addr().unwrap().port();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sender.send_to(b"litep2p", (Ipv4Addr::LOCALHOST, port)).await.unwrap();

        let mut buf = [0u8; 1500];
        let meta = poll_fn(|cx| receiver.poll_recv(cx, &mut buf)).await.unwrap();

        assert_eq!(meta.dst_ip, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert_eq!(meta.addr, sender.local_addr().unwrap());
        assert_eq!(&buf[..meta.len], b"litep2p");
    }

    // MacOS does not support binding to anything other than 127.0.0.1 in the loopback range.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn send_sets_source_address() {
        let src_ip = IpAddr::V4(Ipv4Addr::new(127, 3, 3, 7));
        let sender = webrtc_socket().await;
        let port = sender.socket.local_addr().unwrap().port();

        let receiver = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let destination = receiver.local_addr().unwrap();

        loop {
            match sender.try_send_to(b"litep2p", destination, src_ip) {
                Ok(()) => break,
                // `sendmsg` hit a full send buffer, wait for the socket to drain
                Err(e) if e.kind() == io::ErrorKind::WouldBlock =>
                    sender.socket.writable().await.unwrap(),
                Err(e) => panic!("send failed: {e}"),
            }
        }

        let mut buf = [0u8; 1500];
        let (len, from) = receiver.recv_from(&mut buf).await.unwrap();

        assert_eq!(from, SocketAddr::new(src_ip, port));
        assert_eq!(&buf[..len], b"litep2p");
    }
}
