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

//! End-to-end fuzzing of the litep2p WebRTC listener over a real, authenticated connection.
//!
//! # Model
//!
//! A real str0m client ([`client::WebRtcClient`], ICE-controlling and DTLS-active) completes a
//! genuine ICE + DTLS + SCTP handshake against a live `Litep2p` WebRTC listener over loopback. The
//! listener runs once per process on its own thread (see [`server_addr`]).
//!
//! The first byte of each input selects the target and the rest is split into channel-write chunks,
//! one SCTP message per chunk.
//!
//! - **Pre-auth (even selector).** The chunks are written to the pre-negotiated noise channel,
//!   driving the server's noise-channel de-framing (`extract_framed_message` and
//!   `WebRtcMessage::decode` in `on_noise_channel_data`, `opening.rs`) with genuinely decrypted,
//!   SCTP-framed bytes and fuzzer-chosen message boundaries.
//! - **Post-auth (odd selector).** The client first runs the libp2p Noise handshake as the
//!   *responder* ([`client::WebRtcClient::authenticate`]), so the server authenticates it and
//!   promotes the connection to established. The client then opens a real substream channel and
//!   writes the chunks into it, driving multistream-select negotiation
//!   (`on_inbound_opening_channel_data` and `webrtc_listener_negotiate`) and the substream data
//!   path on a channel with a real SCTP stream id.
//!
//! The post-auth path is why this harness exists. It reaches the authenticated substream layer no
//! in-process harness otherwise can, because litep2p's WebRTC transport is listen-only and never
//! runs the client half of the handshake. The stateless parsers behind both paths are also covered,
//! far faster, by `fuzz/webrtc-codec`, and the substream state machine by `fuzz/webrtc-state`
//! (against a faked channel).
//!
//! # Cost and reproducibility
//!
//! One full DTLS/SCTP handshake per input (plus a Noise handshake on the post-auth path) makes this
//! far slower than the stateless targets, and the handshakes pull from a CSPRNG, so crashes are not
//! byte-exact reproducible from a single input. It is an integration target that complements, not
//! replaces, `fuzz/webrtc-codec` and `fuzz/webrtc-state`.

mod client;

use client::WebRtcClient;

use litep2p::{
    config::ConfigBuilder,
    crypto::ed25519::{Keypair, SecretKey},
    protocol::libp2p::ping,
    transport::webrtc::{config::Config, DtlsCertificate},
    Litep2p, Litep2pEvent,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicUsize, Ordering},
        OnceLock,
    },
    time::Duration,
};

/// Maximum channel-0 write chunks per fuzz input.
const MAX_CHUNKS: usize = 16;

/// How long to wait for the handshake before abandoning an input.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to let the server process the written chunks before dropping the connection.
const DRAIN: Duration = Duration::from_millis(50);

/// How long to wait for a post-auth substream channel to open.
const SUBSTREAM_TIMEOUT: Duration = Duration::from_secs(5);

/// Fixed node identity, so the server's peer id is stable across runs.
const NODE_KEY: [u8; 32] = [7u8; 32];

/// Fixed client identity, so the fuzz client's peer id is stable across runs.
const CLIENT_ID: [u8; 32] = [9u8; 32];

/// Count of connections the server has fully established (Noise handshake completed). The
/// end-to-end tests read it to confirm authentication succeeded.
static ESTABLISHED: AtomicUsize = AtomicUsize::new(0);

/// Build the client's libp2p identity keypair from the fixed secret.
fn client_identity() -> Keypair {
    SecretKey::try_from_bytes(&mut CLIENT_ID.clone()).expect("valid client identity key").into()
}

fn main() {
    // A panic anywhere (the client, the fuzz loop, or the server thread) must abort the process so
    // AFL records it. A thread panic would otherwise only unwind that one thread and be lost.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("webrtc-datagram: panic: {info}");
        std::process::abort();
    }));

    let server = server_addr();
    let id_keys = client_identity();

    ziggy::fuzz!(|data: &[u8]| {
        // The first byte selects the target; the rest is split into channel write chunks.
        let Some((&mode, rest)) = data.split_first() else {
            return;
        };
        let chunks = channel_chunks(rest);
        if chunks.is_empty() {
            return;
        }

        client_runtime().block_on(async {
            // A fresh client, and therefore a fresh server-side connection, per input.
            let mut client = match WebRtcClient::connect(server).await {
                Ok(client) => client,
                // A transient loopback bind hiccup is an environment problem, not a finding.
                Err(_) => return,
            };

            // Every input first completes the handshake, leaving the server past DTLS with its
            // noise channel open. A failure here is a handshake/environment outcome, not a finding.
            let first = match client.handshake(HANDSHAKE_TIMEOUT).await {
                Ok(first) => first,
                Err(_) => {
                    client.close().await;
                    return;
                }
            };

            if mode % 2 == 0 {
                // Pre-auth target: write the chunks to the noise channel, driving the server's
                // `on_noise_channel_data` framing and decode with fuzzer-chosen message boundaries.
                for &chunk in &chunks {
                    if client.write_chunk(chunk).await.is_err() {
                        break;
                    }
                }
            } else {
                // Post-auth target: run the Noise responder to authenticate, then open a substream
                // and feed the chunks into it, driving multistream-select negotiation and the
                // substream data path on a real channel with a real SCTP stream id.
                if client.authenticate(&id_keys, &first).await.is_err() {
                    client.close().await;
                    return;
                }
                let channel = match client.open_substream(SUBSTREAM_TIMEOUT).await {
                    Ok(channel) => channel,
                    Err(_) => {
                        client.close().await;
                        return;
                    }
                };
                for &chunk in &chunks {
                    if client.write_to(channel, chunk).await.is_err() {
                        break;
                    }
                }
            }

            // Let the server process the writes (it may reject a frame and close the connection).
            let _ = client.pump_for(DRAIN).await;
            client.close().await;
        });
    });
}

/// One current-thread runtime for the client side, built once and reused across inputs.
fn client_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap_or_else(|error| harness_failure(&format!("client runtime: {error}")))
    })
}

/// Split fuzzer input into channel-0 chunks on fuzzer-chosen length-byte boundaries.
///
/// Each chunk is at most 255 bytes, and there are at most [`MAX_CHUNKS`] of them. The server
/// concatenates them in its reassembly buffer, so the boundaries are exactly the SCTP message
/// splits the fuzzer controls, which is what exercises `extract_framed_message`'s cross-message
/// reassembly.
fn channel_chunks(mut data: &[u8]) -> Vec<&[u8]> {
    let mut chunks = Vec::new();
    while let Some((len, rest)) = data.split_first() {
        if chunks.len() == MAX_CHUNKS {
            break;
        }
        let len = std::cmp::min(*len as usize, rest.len());
        let (chunk, rest) = rest.split_at(len);
        chunks.push(chunk);
        data = rest;
    }
    chunks
}

/// Stop with a message that cannot be mistaken for a finding.
///
/// A signal (panic/abort) makes AFL file the current input as a crash. A plain non-zero exit is not
/// recorded as one, so a harness or environment failure stays out of the crash directory instead of
/// masquerading as a litep2p bug.
fn harness_failure(what: &str) -> ! {
    eprintln!("webrtc-datagram: harness failure: {what}");
    eprintln!("this is a harness or environment problem, not a finding in litep2p");
    std::process::exit(70);
}

/// Address of the shared litep2p WebRTC listener used by the end-to-end client.
///
/// The listener runs on its own thread with its own runtime, built once for the whole process, so
/// the fuzz client can drive a real handshake against it over loopback. Panics in that thread are
/// turned into process aborts by the panic hook installed in `main`, so a server-side crash is
/// still reported by the fuzzer.
fn server_addr() -> SocketAddr {
    static ADDR: OnceLock<SocketAddr> = OnceLock::new();

    *ADDR.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<SocketAddr>(1);

        std::thread::Builder::new()
            .name("webrtc-e2e-server".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap_or_else(|error| harness_failure(&format!("server runtime: {error}")));

                runtime.block_on(async move {
                    let (ping_config, ping_events) = ping::Config::default();

                    // Generated once for the process, not committed. Nothing here reads the
                    // certhash: the client turns fingerprint verification off and picks up the
                    // server's real fingerprint at runtime, so a fresh certificate per process
                    // behaves identically and saves committing a private key.
                    let certificate = DtlsCertificate::new()
                        .unwrap_or_else(|error| {
                            harness_failure(&format!("DTLS certificate: {error}"))
                        });

                    let config = ConfigBuilder::new()
                        .with_keypair(
                            SecretKey::try_from_bytes(&mut NODE_KEY.clone())
                                .expect("valid key")
                                .into(),
                        )
                        .with_webrtc(Config {
                            listen_addresses: vec!["/ip4/127.0.0.1/udp/0/webrtc-direct"
                                .parse()
                                .expect("valid multiaddress")],
                            certificate: Some(certificate),
                            ..Default::default()
                        })
                        .with_libp2p_ping(ping_config)
                        .build();

                    let mut litep2p = Litep2p::new(config)
                        .unwrap_or_else(|error| harness_failure(&format!("litep2p: {error}")));

                    // Keep the ping protocol registered. The pre-auth target never authenticates
                    // and the post-auth one negotiates but does not speak ping, so no ping events
                    // are produced and the stream never needs draining.
                    let _ping_events = ping_events;

                    let address = litep2p
                        .listen_addresses()
                        .next()
                        .expect("webrtc listener to report an address")
                        .clone();
                    let target =
                        socket_address(&address).expect("listen address to contain ip4/udp");
                    tx.send(target).expect("server address channel to be open");

                    loop {
                        match litep2p.next_event().await {
                            Some(Litep2pEvent::ConnectionEstablished { .. }) => {
                                ESTABLISHED.fetch_add(1, Ordering::Relaxed);
                            }
                            Some(_) => {}
                            None =>
                                harness_failure("litep2p event stream ended; the server is dead"),
                        }
                    }
                });
            })
            .unwrap_or_else(|error| harness_failure(&format!("spawn server thread: {error}")));

        rx.recv().unwrap_or_else(|error| harness_failure(&format!("receive server address: {error}")))
    })
}

/// Extract the UDP socket address from a `/ip4|ip6/.../udp/.../webrtc-direct/...` multiaddr.
fn socket_address(address: &multiaddr::Multiaddr) -> Option<SocketAddr> {
    use multiaddr::Protocol;

    let mut iter = address.iter();
    let ip = match iter.next()? {
        Protocol::Ip4(ip) => IpAddr::V4(ip),
        Protocol::Ip6(ip) => IpAddr::V6(ip),
        _ => return None,
    };
    let Protocol::Udp(port) = iter.next()? else {
        return None;
    };

    Some(SocketAddr::new(ip, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real str0m client completes ICE + DTLS + SCTP against the litep2p listener over loopback,
    /// channel 0 opens, and the server sends its first Noise message.
    ///
    /// If this passes, the end-to-end model is sound: anything the client subsequently writes to
    /// channel 0 reaches the server's `on_noise_channel_data` framing path with real decrypted,
    /// SCTP-framed bytes.
    #[test]
    fn client_completes_handshake_and_receives_server_message() {
        let addr = server_addr();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime to build");

        runtime.block_on(async {
            let mut client = WebRtcClient::connect(addr).await.expect("client connects");
            let received = client
                .handshake(Duration::from_secs(15))
                .await
                .expect("handshake completes and server sends its first noise message");
            assert!(
                !received.is_empty(),
                "server must send its first noise message over channel 0",
            );
            client.close().await;
        });
    }

    /// After the handshake the client runs the Noise responder and the server authenticates it,
    /// emitting `ConnectionEstablished`. Everything the post-auth target reaches lives past this
    /// point, so if this stops passing that whole target is silently testing nothing.
    #[test]
    fn client_authenticates_and_server_establishes_connection() {
        let addr = server_addr();
        let id_keys = client_identity();
        let before = ESTABLISHED.load(Ordering::Relaxed);

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime to build");

        runtime.block_on(async {
            let mut client = WebRtcClient::connect(addr).await.expect("client connects");
            let first =
                client.handshake(Duration::from_secs(15)).await.expect("handshake completes");
            client.authenticate(&id_keys, &first).await.expect("noise responder authenticates");

            // The server processes msg2 on its own thread; pump the client while waiting for the
            // ConnectionEstablished event to land.
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while ESTABLISHED.load(Ordering::Relaxed) <= before
                && std::time::Instant::now() < deadline
            {
                let _ = client.pump_for(Duration::from_millis(100)).await;
            }
            assert!(
                ESTABLISHED.load(Ordering::Relaxed) > before,
                "server must establish the connection after the client authenticates",
            );
            client.close().await;
        });
    }

    /// Post-auth, the client opens a substream and writes bytes, so the server runs
    /// multistream-select on a real channel with a real SCTP stream id. The substream opening at
    /// all proves the authenticated path works end to end, and garbage must not panic the server.
    #[test]
    fn post_auth_substream_negotiation_is_reachable() {
        let addr = server_addr();
        let id_keys = client_identity();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime to build");

        runtime.block_on(async {
            let mut client = WebRtcClient::connect(addr).await.expect("client connects");
            let first =
                client.handshake(Duration::from_secs(15)).await.expect("handshake completes");
            client.authenticate(&id_keys, &first).await.expect("authenticates");

            let channel =
                client.open_substream(Duration::from_secs(10)).await.expect("substream opens");

            // Multistream-select-shaped garbage: drives negotiation without completing it.
            for chunk in [&[0x13u8][..], &[0xff; 40][..], &[0x00][..]] {
                let _ = client.write_to(channel, chunk).await;
            }
            let _ = client.pump_for(Duration::from_millis(200)).await;
            client.close().await;
        });
    }

    /// The fuzzer's core promise: writing garbage to the noise channel drives the framing/decode
    /// path and may close that connection, but must not take the listener down. A second handshake
    /// after the garbage must still complete.
    #[test]
    fn garbage_channel_data_does_not_kill_the_listener() {
        let addr = server_addr();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("client runtime to build");

        runtime.block_on(async {
            let mut first = WebRtcClient::connect(addr).await.expect("connect 1");
            first.handshake(Duration::from_secs(15)).await.expect("handshake 1");
            // Non-minimal varint zero, oversized body, and a bare zero: all framing edge cases.
            for chunk in [&[0x80u8, 0x00][..], &[0xff; 64][..], &[0x00][..]] {
                let _ = first.write_chunk(chunk).await;
            }
            let _ = first.pump_for(Duration::from_millis(100)).await;
            first.close().await;

            let mut second = WebRtcClient::connect(addr).await.expect("connect 2");
            let received = second
                .handshake(Duration::from_secs(15))
                .await
                .expect("handshake 2 after garbage: listener survived");
            assert!(!received.is_empty(), "listener still serves new connections after garbage");
            second.close().await;
        });
    }

    /// The chunker must respect its bounds and clamp an over-long length byte.
    #[test]
    fn channel_chunking_is_bounded() {
        assert!(channel_chunks(&[]).is_empty());
        assert_eq!(channel_chunks(&[0]), vec![&[] as &[u8]], "zero-length chunk is expressible");
        assert_eq!(channel_chunks(&[2, 0xaa, 0xbb, 1, 0xcc]), vec![&[0xaa, 0xbb][..], &[0xcc][..]]);
        // A length byte larger than what remains is clamped rather than dropped.
        assert_eq!(channel_chunks(&[200, 0xaa]), vec![&[0xaa][..]]);
        assert!(channel_chunks(&[0; 1024]).len() <= MAX_CHUNKS);
    }
}
