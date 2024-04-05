# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] - 2023-09-05

This is the second release of litep2p, v0.2.0. The quality of the first release was so bad that this release is a complete rewrite of the library.

Support is added for the following features:

* Transport protocols:
  * TCP
  * QUIC
  * WebRTC
  * WebSocket

* Protocols:
  * [`/ipfs/identify/1.0.0`](https://github.com/libp2p/specs/tree/master/identify)
  * [`/ipfs/ping/1.0.0`](https://github.com/libp2p/specs/blob/master/ping/ping.md)
  * [`/ipfs/kad/1.0.0`](https://github.com/libp2p/specs/tree/master/kad-dht)
  * [`/ipfs/bitswap/1.2.0`](https://github.com/ipfs/specs/blob/main/BITSWAP.md)
  * Request-response protocol
  * Notification protocol
  * Multicast DNS
  * API for creating custom protocols

This time the architecture has been designed to be extensible and integrating new transport and/or user-level protocols should be easier. Additionally, the test coverage is higher both in terms of unit and integration tests. The project also contains conformance tests which test the behavior of `litep2p` against, [`rust-libp2p`](https://github.com/libp2p/rust-libp2p/), [`go-libp2p`](https://github.com/libp2p/go-libp2p/) and Substrate's [`sc-network`](https://github.com/paritytech/polkadot-sdk/tree/master/substrate/client/network). Currently the Substrate conformance tests are not enabled by default as they require unpublished/unaccepted changes to Substrate.

## [0.1.0] - 2023-04-04

This is the first release of `litep2p`, v0.1.0.

Support is added for the following:

* TCP + Noise + Yamux (compatibility with `libp2p`)
* [`/ipfs/identify/1.0.0`](https://github.com/libp2p/specs/tree/master/identify)
* [`/ipfs/ping/1.0.0`](https://github.com/libp2p/specs/blob/master/ping/ping.md)
* Request-response protocol
* Notification protocol

The code quality is atrocious but it works and the second release focuses on providing high test coverage for the library. After that is done and most of the functionality is covered (unit, integration and conformance tests, benchmarks), the focus can be turned to refactoring the code into something clean and efficient.
