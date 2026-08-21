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

//! Datagram-level fuzzing of a live litep2p WebRTC listener.
//!
//! # Status: reaches connection setup, not the DTLS-protected layers
//!
//! Read this before trusting its output. With the committed STUN seed corpus (see `gen_seeds`) a
//! seed datagram now clears the demux: it passes `is_stun_packet`, `StunMessage::parse` and
//! `split_username`, so `make_rtc` runs and an `OpeningWebRtcConnection` is inserted. litep2p does
//! not validate the ufrag (`mod.rs:404-442`), so an arbitrary `ufrag:pass` is enough. What the
//! harness still cannot reach is everything behind the DTLS handshake, and much of what it does
//! reach past the gate is str0m's ICE code rather than litep2p's:
//!
//! - **Nothing gets past the DTLS handshake.** Creating the `Rtc` and feeding it the first STUN
//!   message is as far as a seed goes. Completing DTLS needs a real peer's key agreement, and the
//!   server's per-run crypto makes a captured or mutated handshake desync at once, so
//!   `opening.on_input` beyond the first flight and the whole SCTP and channel-data path stay
//!   unreached. Without a seed the gate itself is unreachable: the STUN magic cookie alone is 2⁻³²
//!   to hit by blind mutation, which is why the corpus is mandatory here.
//! - **The resource-exhaustion angle is thin.** The connection table is keyed by `AddressPair`,
//!   and this harness varies only the source among `NUM_SENDERS` sockets, so the table tops out at
//!   8 opening connections however many STUN seeds run. Running under `ulimit -v` proves nothing.
//! - **Zero-length datagrams never reach `on_socket_input`.** quinn-udp reports
//!   `len = 0, stride = 0`, the transport normalises that to `meta.len = 0`, and the
//!   de-coalescing loop `while offset < meta.len` runs zero times. The empty-buffer guard
//!   inside `on_socket_input` is unreachable from a socket.
//! - **The GRO stride arithmetic never runs.** Every datagram this chunker can emit is at most
//!   255 bytes, and GRO only coalesces a same-4-tuple burst.
//! - **The `datagram_buffer_size` drop-on-full path never runs.** It lives behind
//!   `self.open`, which is populated only after a completed DTLS handshake and Noise identity
//!   exchange.
//!
//! What does run: the socket read path, the listener's address bookkeeping, `is_stun_packet`,
//! `DatagramRecv::try_from` and the first `StunMessage::parse` rejection. Almost all of that
//! is str0m's code, so a crash there belongs upstream.
//!
//! # What would take it further
//!
//! The seed corpus (done, via `gen_seeds`) gets mutation past the STUN gate. Two things still cap
//! its depth. First, the harness assigns each datagram in an iteration to a different source socket
//! (`selector + index`), so a STUN request and a following record never share an `AddressPair` and
//! cannot drive one connection together; letting the fuzzer reuse a source with repetition would
//! fix that. Second, the source pool is fixed at 8. Even with both, litep2p's own WebRTC parsers
//! are covered properly by `fuzz/webrtc-codec` and its state machines by `fuzz/webrtc-state`, which
//! is where the coverage-per-exec is worth having.
//!
//! # Reproducibility
//!
//! Low, and pinning the certificate does not change that. The fixture makes the certhash and
//! peer id stable, which is worth having, but the listener and sender ports are ephemeral and
//! they are what keys the connection table and the ICE candidates; `Instant::now()` feeds every
//! `Rtc` timeout; and node state persists across iterations by design. Replay through
//! `cargo ziggy run` feeds one file to a fresh process, so a crash that depended on earlier
//! iterations cannot be reproduced from a single input at all.

mod fixture;

use litep2p::{
    config::ConfigBuilder,
    crypto::ed25519::SecretKey,
    protocol::libp2p::ping,
    transport::webrtc::{config::Config, DtlsCertificate},
    Litep2p,
};
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    sync::{Mutex, OnceLock},
    time::Duration,
};

/// Distinct source sockets used to reach the listener.
///
/// Each source address is a separate address pair to the transport, and therefore would be a
/// separate `Rtc` in its connection table. Eight is far below any file-descriptor limit, so
/// the number is not the constraint; the constraint is that no `Rtc` is ever created. Raising
/// this only helps once a STUN seed exists. See the module docs.
const NUM_SENDERS: usize = 8;

/// Datagrams per fuzz iteration.
const MAX_DATAGRAMS: usize = 16;

/// How many times the transport is polled after a batch of datagrams.
///
/// Every pass costs one 1ms step of virtual time and no wall-clock time.
const POLL_BUDGET: usize = 8;

/// Fixed node identity, so the peer ID is stable across runs like the DTLS certificate is.
const NODE_KEY: [u8; 32] = [7u8; 32];

/// Long-lived harness state.
///
/// Standing the whole transport up per iteration would dominate the runtime and churn
/// through ports, so the node is built once and reused. The cost is that state bleeds
/// between iterations; that is inherent to fuzzing a live listener and is why the
/// stateless parser targets live in their own crate.
struct Node {
    runtime: tokio::runtime::Runtime,
    litep2p: Litep2p,
    ping: Box<dyn futures::Stream<Item = ping::PingEvent> + Send + Unpin>,
    target: SocketAddr,
    senders: Vec<UdpSocket>,
}

static NODE: OnceLock<Mutex<Node>> = OnceLock::new();

fn main() {
    ziggy::fuzz!(|data: &[u8]| {
        let Some((selector, data)) = data.split_first() else {
            return;
        };

        let mut node = NODE.get_or_init(|| Mutex::new(build_node())).lock().expect("lock");

        for (index, datagram) in datagrams(data).into_iter().enumerate() {
            let sender = &node.senders[(*selector as usize + index) % NUM_SENDERS];
            // A send failure is the harness's problem, not a finding — ignore it and keep
            // the iteration going.
            let _ = sender.send_to(datagram, node.target);
        }

        if !drive(&mut node) {
            harness_failure("the litep2p event stream terminated; the node is dead and every \
                             further iteration would be a no-op");
        }
    });
}

/// Split the input into datagrams on fuzzer-chosen boundaries.
///
/// Sequences matter here: a plausible STUN binding request followed by DTLS bytes reaches
/// further into the transport than either datagram alone, and only the fuzzer knows where
/// to put the boundary.
fn datagrams(mut data: &[u8]) -> Vec<&[u8]> {
    let mut datagrams = Vec::new();

    while let Some((len, rest)) = data.split_first() {
        if datagrams.len() == MAX_DATAGRAMS {
            break;
        }

        // A zero-length datagram is deliberately reachable: it is the input the
        // empty-buffer guard in `on_socket_input` exists to stop reaching str0m.
        let len = std::cmp::min(*len as usize, rest.len());
        let (datagram, rest) = rest.split_at(len);
        datagrams.push(datagram);
        data = rest;
    }

    datagrams
}

/// Let the transport process what was just sent, returning whether the node is still alive.
///
/// Two things to know about the timing. `biased;` matters: without it `tokio::select!` picks a
/// randomised order, and since both the socket wake and the elapsed sleep are ready after the
/// first park, a meaningful fraction of iterations would take the sleep branch and never poll
/// the transport at all. Datagrams would not be lost, but which iteration processed which
/// datagram would be nondeterministic, and that is precisely what a crash reproducer needs to
/// be stable.
///
/// The paused clock only makes this loop's own 1ms budget free. It does **not** make the
/// transport's timers fire eagerly: litep2p's WebRTC timeouts are `futures_timer::Delay` with
/// deadlines from `std::time::Instant::now()`, and neither is affected by tokio's clock. The
/// ICE and DTLS timeout paths are therefore not reached here.
fn drive(node: &mut Node) -> bool {
    let Node {
        runtime,
        litep2p,
        ping,
        ..
    } = node;

    runtime.block_on(async {
        for _ in 0..POLL_BUDGET {
            tokio::select! {
                biased;

                event = litep2p.next_event() => {
                    // `None` means the transport stream ended or the installed protocols
                    // terminated. Every later iteration would be a no-op at high exec/s.
                    if event.is_none() {
                        return false;
                    }
                }
                // The ping stream must be drained too, or its channel fills and applies
                // backpressure that would mask what the fuzzer is doing.
                _event = futures::StreamExt::next(ping) => {}
                _ = tokio::time::sleep(Duration::from_millis(1)) => {}
            }
        }

        true
    })
}

/// Stop with a message that cannot be mistaken for a finding.
///
/// A signal would make AFL file the current input as a crash. A plain non-zero exit is not
/// recorded as one, so a harness or environment failure stays out of the crash directory
/// instead of masquerading as a litep2p bug.
fn harness_failure(what: &str) -> ! {
    eprintln!("webrtc-datagram: harness failure: {what}");
    eprintln!("this is a harness or environment problem, not a finding in litep2p");
    std::process::exit(70);
}

fn build_node() -> Node {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .unwrap_or_else(|error| harness_failure(&format!("tokio runtime: {error}")));

    let (ping_config, ping_events) = ping::Config::default();

    // A committed certificate keeps the certhash — and therefore the advertised
    // multiaddr — identical across runs, matching how the `webrtc-interop` CI job pins the
    // server identity with `--node-key secret`. Without it every run would have a fresh
    // identity and crash reproducers would not replay.
    let certificate = DtlsCertificate::load(fixture::CERTIFICATE.to_vec(), fixture::PRIVATE_KEY.to_vec())
        .expect("fixture certificate to load");

    let config = ConfigBuilder::new()
        .with_keypair(SecretKey::try_from_bytes(&mut NODE_KEY.clone()).expect("valid key").into())
        // Port 0 so the OS picks a free port; the in-tree `tests/webrtc.rs` hardcodes a
        // LAN address and a fixed port, which is unusable here.
        .with_webrtc(Config {
            listen_addresses: vec![
                "/ip4/127.0.0.1/udp/0/webrtc-direct".parse().expect("valid multiaddress"),
            ],
            certificate: Some(certificate),
            ..Default::default()
        })
        // At least one protocol must be registered, otherwise every negotiation is
        // rejected before it reaches the substream code.
        .with_libp2p_ping(ping_config)
        .build();

    let litep2p = runtime.block_on(async {
        Litep2p::new(config).unwrap_or_else(|error| harness_failure(&format!("litep2p: {error}")))
    });

    let address = litep2p
        .listen_addresses()
        .next()
        .expect("webrtc listener to report an address")
        .clone();
    let target = socket_address(&address).expect("listen address to contain ip4/udp");

    let senders = (0..NUM_SENDERS)
        .map(|_| {
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                .unwrap_or_else(|error| harness_failure(&format!("sender socket: {error}")))
        })
        .collect();

    Node {
        runtime,
        litep2p,
        ping: ping_events,
        target,
        senders,
    }
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

    /// The fixture must be a certificate str0m accepts, and the listener must actually
    /// bind — if either fails, every fuzz iteration panics in setup and the harness
    /// reports a "crash" that has nothing to do with the code under test.
    ///
    /// The certhash assertion is the load-bearing one: it is what proves the committed
    /// fixture is actually in use. Without it this test passes just as happily when the
    /// certificate is left to be generated fresh on every start, silently giving up the
    /// cross-run reproducibility the fixture exists to provide.
    #[test]
    fn node_builds_and_listens_on_the_fixture_certificate() {
        let node = build_node();

        assert_eq!(node.target.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(node.target.port(), 0, "port 0 must be resolved to a bound port");
        assert_eq!(node.senders.len(), NUM_SENDERS);

        let expected = DtlsCertificate::load(
            fixture::CERTIFICATE.to_vec(),
            fixture::PRIVATE_KEY.to_vec(),
        )
        .expect("fixture to load")
        .certhash_b64();

        let advertised = node
            .litep2p
            .listen_addresses()
            .next()
            .expect("listen address")
            .to_string();

        assert!(
            advertised.contains(&expected),
            "advertised address {advertised} does not carry the fixture certhash {expected}; \
             the node is using a freshly generated certificate",
        );
    }

    /// Datagrams must reach the listener without taking the transport down.
    ///
    /// The liveness check is `drive`'s return value, not `listen_addresses()`. That method
    /// iterates a `Vec` built once at construction and never updated when a transport dies, so
    /// asserting on it is unconditionally true and cannot fail — the previous version of this
    /// test verified nothing.
    ///
    /// The zero-length datagram is kept because it is free, but note it never reaches
    /// `on_socket_input` at all; see the module docs.
    #[test]
    fn adversarial_datagrams_do_not_kill_the_listener() {
        let mut node = build_node();

        let inputs: Vec<Vec<u8>> = vec![
            vec![],                       // empty: never reaches `on_socket_input`
            vec![0x00],                   // one byte, below any header length
            vec![0x00; 20],               // STUN-length zeros, invalid magic cookie
            vec![0xff; 1500],             // full-MTU garbage
            {
                // STUN-shaped: type 0x0001, length 0, correct magic cookie.
                let mut stun = vec![0x00, 0x01, 0x00, 0x00, 0x21, 0x12, 0xa4, 0x42];
                stun.extend_from_slice(&[0xab; 12]); // transaction id
                stun
            },
            vec![0x16, 0xfe, 0xfd, 0x00, 0x00], // DTLS-shaped handshake prefix
        ];

        for input in &inputs {
            let _ = node.senders[0].send_to(input, node.target);
            assert!(
                drive(&mut node),
                "the transport stream terminated on a {}-byte datagram",
                input.len(),
            );
        }
    }

    /// The chunker must respect its bounds and be able to emit an empty datagram, even though
    /// an empty datagram stops at the socket layer.
    #[test]
    fn datagram_chunking_is_bounded() {
        assert!(datagrams(&[]).is_empty());
        assert_eq!(datagrams(&[0]), vec![&[] as &[u8]], "zero-length datagram is reachable");
        assert_eq!(datagrams(&[2, 0xaa, 0xbb, 1, 0xcc]), vec![&[0xaa, 0xbb][..], &[0xcc][..]]);
        // A length byte larger than what remains is clamped rather than dropped.
        assert_eq!(datagrams(&[200, 0xaa]), vec![&[0xaa][..]]);
        assert!(datagrams(&[0; 1024]).len() <= MAX_DATAGRAMS);
    }
}
