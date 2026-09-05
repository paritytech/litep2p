// Copyright 2026 litep2p developers
//
// Licensed under the same terms as the rest of this repository; see `src/main.rs`.

//! A minimal libp2p-webrtc *client* built on a raw str0m `Rtc`.
//!
//! litep2p's WebRTC transport is listen-only (`dial()` is `NotSupported`), so to exercise the
//! server through a real handshake we drive the other end ourselves. This client is ICE-controlling
//! and DTLS-active (the opposite roles of litep2p's ice-lite listener). Once ICE, DTLS and SCTP are
//! up, the pre-negotiated channel 0 opens and the server (acting as the Noise dialer per the spec)
//! sends its first handshake message. From that point anything the client writes to channel 0
//! arrives at the server's `on_noise_channel_data` as genuinely decrypted, SCTP-framed bytes, which
//! is the path the fuzzer targets.
//!
//! Getting channel 0 open is enough for the pre-auth target, whose bytes reach the framing and
//! decode path behind DTLS. [`WebRtcClient::authenticate`] goes further and runs the libp2p Noise
//! handshake as the *responder*, so the listener authenticates the client and the post-auth
//! substream layer becomes reachable. Everything beyond that (identify, ping, real protocols) is
//! deliberately absent.

use std::{
    net::{Ipv4Addr, SocketAddr},
    time::{Duration, Instant},
};

use str0m::{
    channel::{ChannelConfig, ChannelId, Reliability},
    config::Fingerprint,
    ice::IceCreds,
    net::{Protocol as Str0mProtocol, Receive},
    Candidate, Event, IceConnectionState, Input, Output, Rtc, RtcError,
};
use tokio::net::UdpSocket;

use bytes::BytesMut;
use litep2p::{
    crypto::{ed25519::Keypair, noise::NoiseContext},
    transport::webrtc::util::{extract_framed_message, WebRtcMessage},
};

/// Shared ICE ufrag and password.
///
/// litep2p's `make_rtc` adopts `(ufrag, pass)` verbatim from the STUN username the client sends and
/// installs them as *both* its local and remote ICE credentials. STUN MESSAGE-INTEGRITY on the
/// server is then verified with a password equal to the client's local ufrag. Setting every
/// credential to one shared string makes every integrity check on both sides line up, so ICE
/// completes. The value is longer than the ICE password minimum.
const ICE_CRED: &str = "litep2pfuzzicecredential0";

/// Reasons a client run stops early. None of these is a litep2p finding on its own; they are
/// handshake or environment outcomes the harness treats as "skip this input". The payloads are
/// carried for `Debug` output when diagnosing a stuck campaign, not read on the hot path.
#[allow(dead_code)]
#[derive(Debug)]
pub enum ClientError {
    Io(std::io::Error),
    Rtc(RtcError),
    BadDatagram,
    Disconnected,
    ChannelGone,
    Timeout,
    Noise,
    Framing,
    NoRemoteFingerprint,
}

/// A live str0m client aimed at a litep2p WebRTC listener over loopback UDP.
pub struct WebRtcClient {
    rtc: Rtc,
    socket: UdpSocket,
    local: SocketAddr,
    channel: ChannelId,
    channel_open: bool,
    /// Every channel id str0m has reported open, so `open_substream` can wait for a specific one.
    opened: Vec<ChannelId>,
    recv_buf: Vec<u8>,
}

impl WebRtcClient {
    /// Bind a loopback socket and build a controlling, DTLS-active client aimed at `target`.
    pub async fn connect(target: SocketAddr) -> Result<Self, ClientError> {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.map_err(ClientError::Io)?;
        let local = socket.local_addr().map_err(ClientError::Io)?;
        let (rtc, channel) = build_client_rtc(local, target);
        Ok(Self {
            rtc,
            socket,
            local,
            channel,
            channel_open: false,
            opened: Vec::new(),
            recv_buf: vec![0u8; 2048],
        })
    }

    /// Drain str0m's output queue: send every `Transmit`, record channel events, and return the
    /// next timeout deadline str0m asks to be woken at.
    async fn drain(&mut self, received: &mut Vec<Vec<u8>>) -> Result<Instant, ClientError> {
        loop {
            match self.rtc.poll_output().map_err(ClientError::Rtc)? {
                Output::Timeout(deadline) => return Ok(deadline),
                Output::Transmit(transmit) => {
                    self.socket
                        .send_to(&transmit.contents[..], transmit.destination)
                        .await
                        .map_err(ClientError::Io)?;
                }
                Output::Event(event) => match event {
                    Event::ChannelOpen(id, _) => {
                        if id == self.channel {
                            self.channel_open = true;
                        }
                        self.opened.push(id);
                    }
                    Event::ChannelData(data) if data.id == self.channel => received.push(data.data),
                    Event::IceConnectionStateChange(IceConnectionState::Disconnected) =>
                        return Err(ClientError::Disconnected),
                    _ => {}
                },
            }
        }
    }

    /// One drive step: drain output, then feed either a received datagram or a timeout.
    async fn step(&mut self, received: &mut Vec<Vec<u8>>) -> Result<(), ClientError> {
        let deadline = self.drain(received).await?;
        if !self.rtc.is_alive() {
            return Err(ClientError::Disconnected);
        }

        // Cap the idle wait: str0m returns a far-future deadline when nothing is pending, but we
        // still want to poll the socket promptly during the fast loopback handshake.
        let wait = deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(200));

        match tokio::time::timeout(wait, self.socket.recv_from(&mut self.recv_buf)).await {
            Ok(Ok((len, source))) => {
                // Copy out so the borrow of `recv_buf` does not overlap the `&mut rtc` below.
                let datagram = self.recv_buf[..len].to_vec();
                let receive = Receive {
                    proto: Str0mProtocol::Udp,
                    source,
                    destination: self.local,
                    contents: (&datagram[..]).try_into().map_err(|_| ClientError::BadDatagram)?,
                };
                self.rtc
                    .handle_input(Input::Receive(Instant::now(), receive))
                    .map_err(ClientError::Rtc)?;
            }
            Ok(Err(error)) => return Err(ClientError::Io(error)),
            Err(_elapsed) => self
                .rtc
                .handle_input(Input::Timeout(Instant::now()))
                .map_err(ClientError::Rtc)?,
        }

        Ok(())
    }

    /// Pump until channel 0 is open *and* the server has sent its first Noise message, which is the
    /// point at which the server is waiting in `on_noise_channel_data` for the peer's reply.
    /// Returns whatever the server sent during setup.
    pub async fn handshake(&mut self, timeout: Duration) -> Result<Vec<Vec<u8>>, ClientError> {
        let mut received = Vec::new();
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.channel_open && !received.is_empty() {
                return Ok(received);
            }
            self.step(&mut received).await?;
        }
        Err(ClientError::Timeout)
    }

    /// Complete the libp2p Noise handshake as the responder, so litep2p authenticates the client
    /// and promotes the connection to established, after which real substreams become reachable.
    ///
    /// `server_first_message` is what [`WebRtcClient::handshake`] collected: the channel-0 bytes
    /// carrying the server's first Noise message. `id_keys` is the client's libp2p identity.
    pub async fn authenticate(
        &mut self,
        id_keys: &Keypair,
        server_first_message: &[Vec<u8>],
    ) -> Result<(), ClientError> {
        // The prologue must byte-match the server's: "libp2p-webrtc-noise:" then the client's cert
        // multihash, then the server's. str0m exposes the real DTLS fingerprints once the handshake
        // completes; each is a 32-byte SHA-256 wrapped as a sha2-256 multihash.
        let client_fp = self.rtc.direct_api().local_dtls_fingerprint().bytes.clone();
        let server_fp = self
            .rtc
            .direct_api()
            .remote_dtls_fingerprint()
            .ok_or(ClientError::NoRemoteFingerprint)?
            .bytes
            .clone();
        let prologue = webrtc_noise_prologue(&client_fp, &server_fp);

        let mut noise = NoiseContext::with_prologue_responder(id_keys, prologue)
            .map_err(|_| ClientError::Noise)?;

        // Reassemble the server's first message and strip the outer WebRTC framing to the inner
        // Noise message, exactly as the server does on its side.
        let mut buffer = BytesMut::new();
        for frame in server_first_message {
            buffer.extend_from_slice(frame);
        }
        let body = extract_framed_message(&mut buffer)
            .map_err(|_| ClientError::Framing)?
            .ok_or(ClientError::Framing)?;
        let msg1 = WebRtcMessage::decode(&body)
            .map_err(|_| ClientError::Framing)?
            .payload
            .ok_or(ClientError::Framing)?;

        // Responder: read msg1, produce msg2 carrying our signed identity, wrap and send it.
        noise.read_first_message(&msg1).map_err(|_| ClientError::Noise)?;
        let msg2 = noise.second_message().map_err(|_| ClientError::Noise)?;
        let channel_bytes = WebRtcMessage::encode(msg2, None);

        match self.rtc.channel(self.channel) {
            Some(mut channel) => {
                channel.write(true, &channel_bytes).map_err(ClientError::Rtc)?;
            }
            None => return Err(ClientError::ChannelGone),
        }

        let mut sink = Vec::new();
        self.drain(&mut sink).await?;
        Ok(())
    }

    /// Write one chunk to channel 0 as its own SCTP message, then flush transmits.
    ///
    /// Each call becomes one `Event::ChannelData` and therefore one `on_noise_channel_data` call on
    /// the server, so a sequence of chunks drives its cross-message reassembly with fuzzer-chosen
    /// boundaries.
    pub async fn write_chunk(&mut self, chunk: &[u8]) -> Result<(), ClientError> {
        match self.rtc.channel(self.channel) {
            Some(mut channel) => {
                channel.write(true, chunk).map_err(ClientError::Rtc)?;
            }
            None => return Err(ClientError::ChannelGone),
        }
        let mut sink = Vec::new();
        self.drain(&mut sink).await?;
        Ok(())
    }

    /// Open a fresh SCTP-negotiated data channel (a substream) and wait until it is open.
    ///
    /// Unlike channel 0, this is negotiated over SCTP, so the server sees a `ChannelOpen` and
    /// treats it as a new inbound substream: writing to it drives multistream-select negotiation
    /// and, once that succeeds, the substream data path.
    pub async fn open_substream(&mut self, timeout: Duration) -> Result<ChannelId, ClientError> {
        let id = self.rtc.direct_api().create_data_channel(ChannelConfig {
            label: String::new(),
            ordered: true,
            reliability: Reliability::Reliable,
            negotiated: None,
            protocol: String::new(),
        });

        let deadline = Instant::now() + timeout;
        let mut sink = Vec::new();
        while Instant::now() < deadline {
            if self.opened.contains(&id) {
                return Ok(id);
            }
            self.step(&mut sink).await?;
        }
        Err(ClientError::Timeout)
    }

    /// Write bytes to a specific channel as one SCTP message, then flush transmits.
    pub async fn write_to(&mut self, channel: ChannelId, bytes: &[u8]) -> Result<(), ClientError> {
        match self.rtc.channel(channel) {
            Some(mut channel) => {
                channel.write(true, bytes).map_err(ClientError::Rtc)?;
            }
            None => return Err(ClientError::ChannelGone),
        }
        let mut sink = Vec::new();
        self.drain(&mut sink).await?;
        Ok(())
    }

    /// Pump for a bounded duration so the peer's responses are processed.
    pub async fn pump_for(&mut self, duration: Duration) -> Result<(), ClientError> {
        let deadline = Instant::now() + duration;
        let mut sink = Vec::new();
        while Instant::now() < deadline && self.rtc.is_alive() {
            self.step(&mut sink).await?;
        }
        Ok(())
    }

    /// Best-effort disconnect so the server reclaims its opening connection promptly instead of
    /// waiting for an ICE/DTLS timeout.
    pub async fn close(&mut self) {
        self.rtc.disconnect();
        let mut sink = Vec::new();
        let _ = self.drain(&mut sink).await;
    }
}

/// Build the client `Rtc`: ICE-controlling, DTLS-active, SCTP-initiator, with the pre-negotiated
/// channel 0 that mirrors litep2p's noise channel.
fn build_client_rtc(local: SocketAddr, remote: SocketAddr) -> (Rtc, ChannelId) {
    let creds = IceCreds {
        ufrag: ICE_CRED.to_string(),
        pass: ICE_CRED.to_string(),
    };

    // Verification off, and no explicit DTLS cert: str0m's crypto provider generates one. The
    // client identity is not used here; `authenticate` supplies the libp2p keypair when the
    // post-auth target runs the Noise handshake.
    let mut rtc = Rtc::builder().set_fingerprint_verification(false).build(Instant::now());

    rtc.add_local_candidate(
        Candidate::host(local, Str0mProtocol::Udp).expect("valid local host candidate"),
    );
    rtc.add_remote_candidate(
        Candidate::host(remote, Str0mProtocol::Udp).expect("valid remote host candidate"),
    );

    {
        let mut api = rtc.direct_api();
        api.set_local_ice_credentials(creds.clone());
        api.set_remote_ice_credentials(creds);
        api.set_ice_controlling(true);
        // Required even with verification off: str0m disconnects if no remote fingerprint is set
        // before the peer certificate arrives. The value itself is ignored while verification is
        // disabled.
        api.set_remote_fingerprint(Fingerprint {
            hash_func: "sha-256".to_string(),
            bytes: vec![0u8; 32],
        });
        api.start_dtls(true).expect("start_dtls(active) on a fresh Rtc");
        api.start_sctp(true);
    }

    let channel = rtc.direct_api().create_data_channel(ChannelConfig {
        label: "noise".to_string(),
        ordered: true,
        reliability: Reliability::Reliable,
        negotiated: Some(0),
        protocol: String::new(),
    });

    (rtc, channel)
}

/// The libp2p-webrtc Noise prologue: `"libp2p-webrtc-noise:" ++ mh(client) ++ mh(server)`, where
/// `mh` is the sha2-256 multihash of a DTLS certificate (`[0x12, 0x20]` followed by the 32-byte
/// hash). The client's fingerprint comes first, matching litep2p's `noise_prologue`.
fn webrtc_noise_prologue(client_fp_sha256: &[u8], server_fp_sha256: &[u8]) -> Vec<u8> {
    let multihash = |hash: &[u8]| {
        let mut wrapped = Vec::with_capacity(2 + hash.len());
        wrapped.push(0x12); // sha2-256 code
        wrapped.push(0x20); // 32-byte digest length
        wrapped.extend_from_slice(hash);
        wrapped
    };

    let mut prologue = b"libp2p-webrtc-noise:".to_vec();
    prologue.extend_from_slice(&multihash(client_fp_sha256));
    prologue.extend_from_slice(&multihash(server_fp_sha256));
    prologue
}
