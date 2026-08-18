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
//! # What this actually covers, and what it does not
//!
//! Be clear-eyed about this harness before trusting its output. It reaches litep2p only
//! through the public API — bind a `Litep2p` with a WebRTC listener, then write fuzzed UDP
//! datagrams at the bound port — which means it covers the *pre-handshake* path:
//! `is_stun_packet`, the datagram demux in `on_socket_input` (including its empty-buffer
//! guard, which exists because str0m panics on zero-length input), STUN parsing and
//! username splitting, the GRO stride de-coalescing arithmetic, and the
//! `datagram_buffer_size` drop-on-full path.
//!
//! Everything past that is gated by DTLS, which fuzzer-random bytes will not complete. So:
//!
//! - Most parsing reached here is **str0m's**, not litep2p's. Crashes found in
//!   `DatagramRecv::try_from` or `StunMessage::parse` belong upstream. litep2p's own
//!   parsers are fuzzed properly by `fuzz/webrtc-codec` and `fuzz/webrtc-state`, which is
//!   where coverage-per-exec is worth having.
//! - Coverage feedback is weak and the harness is slow: a real socket, a real transport
//!   and vendored OpenSSL sit in the loop, and node state persists across iterations.
//!
//! Its real value is **resource exhaustion**, not parser bugs — the transport keeps an
//! `Rtc` per remote address pair, so a spray from many source addresses grows that table.
//! Run it under a memory ceiling (e.g. `ulimit -v`) so unbounded growth surfaces as an
//! OOM crash instead of as silence.

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
/// Each source address is a separate address pair to the transport, and therefore a
/// separate `Rtc` in its connection table. A handful is enough to exercise the demux and
/// the table's growth without the harness itself leaking file descriptors.
const NUM_SENDERS: usize = 8;

/// Datagrams per fuzz iteration.
const MAX_DATAGRAMS: usize = 16;

/// How many times the transport is polled after a batch of datagrams.
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

        drive(&mut node);
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

/// Let the transport process what was just sent.
///
/// Virtual time is paused, so a `timeout` on a pending future auto-advances the clock the
/// moment the runtime goes idle: spawned connection tasks still get to run, but the
/// harness never actually waits a millisecond of wall-clock. The side effect is that
/// transport timers fire eagerly, which reaches timeout paths at the cost of some
/// realism.
fn drive(node: &mut Node) {
    let Node {
        runtime,
        litep2p,
        ping,
        ..
    } = node;

    runtime.block_on(async {
        for _ in 0..POLL_BUDGET {
            tokio::select! {
                _event = litep2p.next_event() => {}
                // The ping stream must be drained too, or its channel fills and applies
                // backpressure that would mask what the fuzzer is doing.
                _event = futures::StreamExt::next(ping) => {}
                _ = tokio::time::sleep(Duration::from_millis(1)) => break,
            }
        }
    });
}

fn build_node() -> Node {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .start_paused(true)
        .build()
        .expect("runtime to build");

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

    let litep2p = runtime.block_on(async { Litep2p::new(config).expect("litep2p to start") });

    let address = litep2p
        .listen_addresses()
        .next()
        .expect("webrtc listener to report an address")
        .clone();
    let target = socket_address(&address).expect("listen address to contain ip4/udp");

    let senders = (0..NUM_SENDERS)
        .map(|_| UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).expect("sender socket to bind"))
        .collect();

    Node {
        runtime,
        litep2p,
        ping: Box::new(ping_events),
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

    /// Datagrams must reach the listener without taking the transport down. Includes the
    /// zero-length datagram, which is the specific input litep2p guards against because
    /// str0m panics on it.
    #[test]
    fn adversarial_datagrams_do_not_kill_the_listener() {
        let mut node = build_node();

        let inputs: Vec<Vec<u8>> = vec![
            vec![],                       // empty: must be dropped before reaching str0m
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
            drive(&mut node);
        }

        // The listener must still be advertising an address, i.e. it did not tear down.
        assert!(
            node.litep2p.listen_addresses().next().is_some(),
            "listener must survive adversarial datagrams",
        );
    }

    /// The chunker must respect its bounds and be able to emit an empty datagram.
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
