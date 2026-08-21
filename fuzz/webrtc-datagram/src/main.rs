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

//! End-to-end fuzzing of the litep2p WebRTC listener's Noise-channel framing.
//!
//! # Model
//!
//! A real str0m client ([`client::WebRtcClient`], ICE-controlling and DTLS-active) completes a
//! genuine ICE + DTLS + SCTP handshake against a live `Litep2p` WebRTC listener over loopback. The
//! listener runs once per process on its own thread (see [`server_addr`]).
//!
//! Once the pre-negotiated channel 0 opens, the server acts as the Noise dialer (per the libp2p
//! spec): it sends its first handshake message and then waits in `on_noise_channel_data`
//! (`src/transport/webrtc/opening.rs`) for the reply. The fuzzer input is split into chunks and
//! written to channel 0; each chunk becomes one SCTP message and therefore one
//! `on_noise_channel_data` call, so the fuzzer drives that function's `extract_framed_message`
//! reassembly and `WebRtcMessage::decode` path with genuinely decrypted, SCTP-framed bytes and
//! fuzzer-chosen message boundaries.
//!
//! This is the only harness that reaches that path through a real handshake. The stateless parsers
//! behind it (`extract_framed_message`, `WebRtcMessage::decode`) are also covered directly, and far
//! faster, by `fuzz/webrtc-codec`.
//!
//! # Scope
//!
//! The client never runs the Noise handshake, so it does not authenticate: the authenticated
//! substream layer is out of scope by design (that is "Option 2"). Everything up to and including
//! the server's Noise-channel de-framing is reached.
//!
//! # Cost and reproducibility
//!
//! One full DTLS/SCTP handshake per input makes this far slower than the stateless targets, and the
//! handshake pulls from a CSPRNG, so crashes are not byte-exact reproducible from a single input.
//! It is an integration target that complements, not replaces, `fuzz/webrtc-codec` and
//! `fuzz/webrtc-state`.

mod client;
mod fixture;

use client::WebRtcClient;

use litep2p::{
    config::ConfigBuilder,
    crypto::ed25519::SecretKey,
    protocol::libp2p::ping,
    transport::webrtc::{config::Config, DtlsCertificate},
    Litep2p,
};
use std::{
    net::{IpAddr, SocketAddr},
    sync::OnceLock,
    time::Duration,
};

/// Maximum channel-0 write chunks per fuzz input.
const MAX_CHUNKS: usize = 16;

/// How long to wait for the handshake before abandoning an input.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to let the server process the written chunks before dropping the connection.
const DRAIN: Duration = Duration::from_millis(50);

/// Fixed node identity, so the peer id and certhash are stable across runs like the DTLS fixture.
const NODE_KEY: [u8; 32] = [7u8; 32];

fn main() {
    // A panic anywhere (the client, the fuzz loop, or the server thread) must abort the process so
    // AFL records it. A thread panic would otherwise only unwind that one thread and be lost.
    std::panic::set_hook(Box::new(|info| {
        eprintln!("webrtc-datagram: panic: {info}");
        std::process::abort();
    }));

    let server = server_addr();

    ziggy::fuzz!(|data: &[u8]| {
        let chunks = channel_chunks(data);
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

            // Complete the handshake so the server is parked in `on_noise_channel_data`. A failure
            // here (timeout, disconnect) is a handshake/environment outcome, not a litep2p finding.
            if client.handshake(HANDSHAKE_TIMEOUT).await.is_err() {
                client.close().await;
                return;
            }

            // Drive the target: each chunk is one SCTP message, one `on_noise_channel_data` call.
            for &chunk in &chunks {
                if client.write_chunk(chunk).await.is_err() {
                    break;
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
                    let certificate = DtlsCertificate::load(
                        fixture::CERTIFICATE.to_vec(),
                        fixture::PRIVATE_KEY.to_vec(),
                    )
                    .expect("fixture certificate to load");

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

                    // Keep the ping protocol registered; Option 1 never authenticates, so no ping
                    // events are produced and the stream never needs draining.
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
                        if litep2p.next_event().await.is_none() {
                            harness_failure("litep2p event stream ended; the server is dead");
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

    /// Phase-0 spike: a real str0m client completes ICE + DTLS + SCTP against the litep2p listener
    /// over loopback, channel 0 opens, and the server sends its first Noise message.
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
