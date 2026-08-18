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

//! Test and fuzz scaffolding for [`WebRtcConnection`].
//!
//! A child module so it can reach the connection's private fields and inbound entry points without
//! widening them in production code.

use super::*;

/// A `WebRtcConnection` built from deterministic inputs, with its inbound entry points exposed so
/// `fuzz/webrtc-state` can drive the channel state machine directly.
///
/// With no DTLS handshake, channels are `negotiated: None`, so str0m assigns no SCTP stream id and
/// every `write()` fails; a channel from [`Self::open_channel()`] is therefore one-frame.
/// [`Self::open_negotiated_channel()`] reaches `ChannelState::Open` by installing the substream
/// directly. `fuzz/README.md` has the full reachability matrix.
pub struct FuzzConnection {
    /// Private: `WebRtcConnection::new` takes crate-internal types that exposing it would leak.
    connection: WebRtcConnection,

    /// Channels in creation order, plus whether still open. Never removed, so an index keeps
    /// addressing the same channel across a mutated script, including data delivered after a close.
    channels: Vec<(ChannelId, bool)>,

    /// Local ends of the substreams [`Self::open_negotiated_channel()`] installs, indexed with
    /// `channels`. Held to keep each handle's peer live: dropping a `Substream` starts a half-close
    /// and collapses the `Open` state.
    substreams: Vec<Option<Substream>>,

    /// Held so the connection's datagram receiver does not observe a closed sender.
    _dgram_tx: tokio::sync::mpsc::Sender<Vec<u8>>,

    /// Drained by [`Self::drain_events()`]: both hold 256 events and `ProtocolSet` sends with
    /// `.await`, so an unread receiver would hang on the 257th and read as a timeout, not a finding.
    mgr_rx: tokio::sync::mpsc::Receiver<crate::transport::manager::TransportManagerEvent>,
    protocol_rx: tokio::sync::mpsc::Receiver<crate::protocol::InnerTransportEvent>,
}

#[cfg_attr(not(feature = "fuzz"), allow(dead_code))]
impl FuzzConnection {
    /// Build a connection wired to `protocols`, with no DTLS handshake performed.
    pub async fn new(protocols: Vec<ProtocolName>) -> crate::Result<Self> {
        use crate::{
            codec::ProtocolCodec,
            transport::{manager::ProtocolContext, webrtc::certificate::DtlsCertificate},
            types::ConnectionId,
        };
        use std::sync::atomic::AtomicUsize;
        use str0m::{Candidate, IceCreds};

        let local = "127.0.0.1:4242".parse().expect("valid socket address");
        let remote = "127.0.0.1:4243".parse().expect("valid socket address");
        let addrs = AddressPair { local, remote };

        // Mirrors `WebRtcTransport::make_rtc` minus the handshake. The certificate is generated
        // once per process: keypair generation dominates the build cost and it is never used, since
        // no DTLS handshake happens.
        static DTLS_CERT_DER: std::sync::OnceLock<(Vec<u8>, Vec<u8>)> = std::sync::OnceLock::new();
        let (certificate, private_key) = DTLS_CERT_DER.get_or_init(|| {
            let generated = DtlsCertificate::new().expect("DTLS certificate generation to succeed");
            let (certificate, private_key) = generated.as_parts();

            (certificate.clone(), private_key.clone())
        });
        let dtls_cert: str0m::config::DtlsCert =
            DtlsCertificate::load(certificate.clone(), private_key.clone())?.into();
        let mut rtc = Rtc::builder()
            .set_ice_lite(true)
            .set_dtls_cert(dtls_cert)
            .set_fingerprint_verification(false)
            .build(std::time::Instant::now());
        rtc.add_local_candidate(
            Candidate::host(local, Str0mProtocol::Udp).map_err(str0m::RtcError::Ice)?,
        );
        rtc.add_remote_candidate(
            Candidate::host(remote, Str0mProtocol::Udp).map_err(str0m::RtcError::Ice)?,
        );
        let creds = IceCreds {
            ufrag: "fuzzufrag".to_string(),
            pass: "fuzzpassfuzzpass".to_string(),
        };
        rtc.direct_api().set_remote_ice_credentials(creds.clone());
        rtc.direct_api().set_local_ice_credentials(creds);
        rtc.direct_api().set_ice_controlling(false);

        let (protocol_tx, protocol_rx) = tokio::sync::mpsc::channel(256);
        let (mgr_tx, mgr_rx) = tokio::sync::mpsc::channel(256);
        let (_dgram_tx, dgram_rx) = tokio::sync::mpsc::channel(256);

        let protocols = protocols
            .into_iter()
            .map(|protocol| {
                (
                    protocol,
                    ProtocolContext {
                        codec: ProtocolCodec::Identity(0xffff),
                        tx: protocol_tx.clone(),
                        fallback_names: Vec::new(),
                        keep_alive: SubstreamKeepAlive::No,
                    },
                )
            })
            .collect();

        let connection_id = ConnectionId::from(0usize);
        let protocol_set = ProtocolSet::new(
            connection_id,
            mgr_tx,
            Arc::new(AtomicUsize::new(0)),
            protocols,
        );

        // Placeholder, like the certificate: `addrs` is hardcoded and no I/O runs on it. Bound once
        // per process so a fuzz input costs no syscall and a bind failure is one setup failure.
        static SOCKET: std::sync::OnceLock<Arc<WebRtcSocket>> = std::sync::OnceLock::new();
        let socket = match SOCKET.get() {
            Some(socket) => socket.clone(),
            None => {
                let socket = Arc::new(WebRtcSocket::new(
                    tokio::net::UdpSocket::bind("127.0.0.1:0").await?,
                )?);
                let _ = SOCKET.set(socket.clone());
                socket
            }
        };

        let connection = WebRtcConnection::new(
            rtc,
            PeerId::random(),
            addrs,
            socket,
            protocol_set,
            Endpoint::listener(multiaddr::Multiaddr::empty(), connection_id),
            dgram_rx,
        );

        Ok(Self {
            connection,
            channels: Vec::new(),
            substreams: Vec::new(),
            _dgram_tx,
            mgr_rx,
            protocol_rx,
        })
    }

    /// Open an inbound data channel, returning its index. It lands in `InboundOpening`, so its
    /// first complete frame runs one pass of `webrtc_listener_negotiate` and then closes it.
    pub async fn open_channel(&mut self) -> crate::Result<usize> {
        self.drain_events();

        let label = format!("fuzz-{}", self.channels.len());
        let channel_id = self.connection.rtc.direct_api().create_data_channel(ChannelConfig {
            label: label.clone(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        });

        self.connection.on_channel_opened(channel_id, label).await?;
        self.channels.push((channel_id, true));
        self.substreams.push(None);

        Ok(self.channels.len() - 1)
    }

    /// Feed inbound bytes to the channel at `index` (a no-op if unopened). Data for a *closed*
    /// channel is delivered deliberately, so post-close ordering is decided by the code under test;
    /// the resulting error is the caller's to ignore.
    pub async fn inbound(&mut self, index: usize, data: Vec<u8>) -> crate::Result<()> {
        self.drain_events();

        let Some((channel_id, _open)) = self.channels.get(index).copied() else {
            return Ok(());
        };

        self.connection.on_inbound_data(channel_id, data).await
    }

    /// Close the channel at `index`, if this scaffold still considers it open.
    pub async fn close_channel(&mut self, index: usize) -> crate::Result<()> {
        self.drain_events();

        let Some((channel_id, open)) = self.channels.get_mut(index) else {
            return Ok(());
        };
        if !*open {
            return Ok(());
        }

        *open = false;
        let channel_id = *channel_id;

        self.connection.on_channel_closed(channel_id).await
    }

    /// Install an already-negotiated channel in `ChannelState::Open`, returning its index.
    ///
    /// Runs the tail of `on_inbound_opening_channel_data` directly (multistream-select can't
    /// complete here), with two divergences that keep `Open` alive: `report_substream_open` is
    /// skipped so the local end stays in `substreams`, and `lifetime_permit` is `None`.
    pub fn open_negotiated_channel(&mut self, protocol: ProtocolName) -> crate::Result<usize> {
        self.drain_events();

        let label = format!("fuzz-open-{}", self.channels.len());
        let channel_id = self.connection.rtc.direct_api().create_data_channel(ChannelConfig {
            label,
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        });

        let substream_id = self.connection.protocol_set.next_substream_id();
        let codec = self.connection.protocol_set.protocol_codec(&protocol);
        let _permit =
            self.connection.protocol_set.try_get_permit().ok_or(Error::ConnectionClosed)?;
        let (substream, handle) = WebRtcSubstream::new();
        let substream = Substream::new_webrtc(self.connection.peer, substream_id, substream, codec);

        self.connection.handles.insert(channel_id, handle);
        self.connection.channels.insert(
            channel_id,
            ChannelState::Open {
                substream_id,
                channel_id,
                lifetime_permit: None,
            },
        );

        self.substreams.push(Some(substream));
        self.channels.push((channel_id, true));

        Ok(self.channels.len() - 1)
    }

    /// Poll the handle set once, dispatch anything it yields via `on_handle_message` (as
    /// `run_event_loop` does), and report whether it produced a message. The only route to
    /// `SubstreamHandleSet::poll_next` and its `swap_remove` reordering.
    pub fn poll_handles(&mut self) -> bool {
        self.drain_events();

        let mut context = Context::from_waker(futures::task::noop_waker_ref());

        match self.connection.handles.poll_next_unpin(&mut context) {
            Poll::Ready(Some((channel_id, message))) => {
                self.connection.on_handle_message(channel_id, message);
                true
            }
            Poll::Ready(None) | Poll::Pending => false,
        }
    }

    /// Read from the local end of the substream at `index`, making inbound delivery observable:
    /// reassembly, decode and `SubstreamHandle::on_message` all have to work for a payload to land.
    pub fn read_substream(&mut self, index: usize, len: usize) -> Option<Vec<u8>> {
        use futures::FutureExt;
        use tokio::io::AsyncReadExt;

        let substream = self.substreams.get_mut(index)?.as_mut()?;
        let mut buffer = vec![0u8; len];

        match substream.read(&mut buffer).now_or_never() {
            Some(Ok(read)) => {
                buffer.truncate(read);
                Some(buffer)
            }
            Some(Err(_)) | None => None,
        }
    }

    /// Drain the protocol and manager event queues, returning how many events were dropped. Called
    /// at the start of every operation; see `mgr_rx` for why.
    pub fn drain_events(&mut self) -> usize {
        let mut drained = 0;

        while self.protocol_rx.try_recv().is_ok() {
            drained += 1;
        }
        while self.mgr_rx.try_recv().is_ok() {
            drained += 1;
        }

        drained
    }

    /// Number of channels opened.
    pub fn channel_count(&self) -> usize {
        self.channels.len()
    }

    /// Total bytes across all reassembly buffers. Each is capped at `MAX_FRAME_SIZE`; the sum is not.
    pub fn buffered_bytes(&self) -> usize {
        self.connection.recv_buffers.values().map(|buffer| buffer.len()).sum()
    }

    /// Number of live reassembly buffers.
    pub fn buffer_count(&self) -> usize {
        self.connection.recv_buffers.len()
    }

    /// Largest single reassembly buffer. A buffer only holds bytes mid-reassembly and no frame
    /// exceeds `MAX_FRAME_SIZE`, so growth past that is a defect.
    pub fn max_buffered_bytes(&self) -> usize {
        self.connection
            .recv_buffers
            .values()
            .map(|buffer| buffer.len())
            .max()
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn protocols() -> Vec<ProtocolName> {
        vec![ProtocolName::from("/ipfs/ping/1.0.0")]
    }

    /// Data for a channel with no state must error without creating a reassembly buffer (nothing
    /// would reclaim it: `on_channel_closed` only runs for channels litep2p knows about).
    #[tokio::test]
    async fn post_close_data_creates_no_buffer() {
        let mut connection = FuzzConnection::new(protocols()).await.expect("scaffold to build");
        let index = connection.open_channel().await.expect("channel to open");
        connection.close_channel(index).await.expect("channel to close");

        assert!(
            connection.inbound(index, vec![0xac, 0x02, 0xaa, 0xbb]).await.is_err(),
            "data for a channel with no state must be reported as an error",
        );

        assert_eq!(
            connection.buffer_count(),
            0,
            "a closed channel must not regain a buffer"
        );
    }

    /// An inbound frame on an `Open` channel must traverse the whole path: reassembly, decode,
    /// `SubstreamHandle::on_message`, and out the local `Substream`. This is what makes
    /// `open_negotiated_channel` worth having.
    #[tokio::test]
    async fn open_channel_delivers_payload_to_substream() {
        let mut connection = FuzzConnection::new(protocols()).await.expect("scaffold to build");
        let index = connection
            .open_negotiated_channel(ProtocolName::from("/ipfs/ping/1.0.0"))
            .expect("negotiated channel to install");

        let frame = WebRtcMessage::encode(b"payload".to_vec(), None);
        connection.inbound(index, frame).await.expect("frame to be handled");

        assert_eq!(
            connection.read_substream(index, 32).as_deref(),
            Some(&b"payload"[..]),
            "the payload must reach the local end of the substream",
        );
    }

    /// Polling the handle set must survive the `swap_remove` reordering a close triggers, without
    /// tripping the `insert` assert or the `poll_next` index walk.
    #[tokio::test]
    async fn polling_handles_survives_removal() {
        let mut connection = FuzzConnection::new(protocols()).await.expect("scaffold to build");

        for _ in 0..4 {
            connection
                .open_negotiated_channel(ProtocolName::from("/ipfs/ping/1.0.0"))
                .expect("negotiated channel to install");
        }

        for _ in 0..8 {
            connection.poll_handles();
        }

        // Closing reorders the set through `swap_remove`, so keep polling afterwards.
        connection.close_channel(1).await.expect("channel to close");

        for _ in 0..8 {
            connection.poll_handles();
        }
    }
}
