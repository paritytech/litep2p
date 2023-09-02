# litep2p

[![License](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE) [![Crates.io](https://img.shields.io/crates/v/litep2p.svg)](https://crates.io/crates/litep2p) [![docs.rs](https://img.shields.io/docsrs/litep2p.svg)](https://docs.rs/litep2p/latest/litep2p/)

`litep2p` is a [`libp2p`](https://libp2p.io/)-compatible *peer-to-peer (P2P)* networking library

## Features

* Supported protocols:
  * `/ipfs/ping/1.0.0`
  * `/ipfs/identify/1.0.0`
  * `/ipfs/kad/1.0.0`
  * `/ipfs/bitswap/1.2.0`
  * Multicast DNS
  * Notification protocol
  * Request-response protocol
  * API for creating custom protocols 
* Supported transports:
  * TCP
  * QUIC
  * WebRTC
  * WebSocket

## Usage

`litep2p` has taken a different approach with API design and as such is not a drop-in replacement for `libp2p`. Below is a sample usage of the library:

```rust
use futures::Stream;
use litep2p::{
    config::Litep2pBuilder,
    protocol::{libp2p::ping::PingConfig, request_response::RequestResponseConfig},
    transport::{quic::QuicTransportConfig, tcp::TcpTransportConfig},
};

// simple example which enables `/ipfs/ping/1.0.0` and `/request/1` protocols
// and TCP and QUIC transports and starts polling events
#[tokio::main]
async fn main() {
    // enable IPFS PING protocol
    let (ping_config, mut ping_event_stream) = PingConfig::new().build().unwrap();

    let (req_resp_config, mut req_resp_handle) =
        RequestResponseConfigBuilder::new(ProtocolName::from("/request/1"))
            .with_max_size(1024)
            .build();

    // build `Litep2pConfig` object
    let mut config = Litep2pBuilder::new()
        .with_tcp(TcpTransportConfig {
            listen_address: "/ip6/::1/tcp/0".parse().unwrap(),
            yamux_config: yamux::Config::default(),
        })
        .with_quic(QuicTransportConfig {
            listen_address: "/ip4/127.0.0.1/udp/0/quic-v1".parse().unwrap(),
        })
        .with_ping(ping_config)
        .with_request_response(req_resp_config)
        .build()
        .unwrap();

    // build `Litep2p` object
    let mut litep2p = Litep2p::new(config).await.unwrap();

    loop {
        tokio::select! {
            _event = litep2p.next_event() => {},
            _event = req_resp_handle.next() => {},
            _event = ping_event_stream.next() => {},
        }
    }
}
```

See[`examples`](https://github.com/altonen/lite2p/examples) for more details on how to use the library

## Copying

MIT license