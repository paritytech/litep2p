# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.7.0] - 2024-09-05

This release introduces several new features, improvements, and fixes to the litep2p library. Key updates include enhanced error handling, configurable connection limits, and a new API for managing public addresses.

### [Exposing Public Addresses API](https://github.com/paritytech/litep2p/pull/212)

A new `PublicAddresses` API has been added, enabling users to manage the node's public addresses. This API allows developers to add, remove, and retrieve public addresses, which are shared with peers through the Identify protocol.

```rust
    // Public addresses are accessible from the main litep2p instance.
    let public_addresses = litep2p.public_addresses();

    // Add a new public address to the node.
    if let Err(err) = public_addresses.add_address(multiaddr) {
        eprintln!("Failed to add public address: {:?}", err);
    }

    // Remove a public address from the node.
    public_addresses.remove_address(&multiaddr);

    // Retrieve all public addresses of the node.
    for address in public_addresses.get_addresses() {
        println!("Public address: {}", address);
    }
```

**Breaking Change**: The Identify protocol no longer includes public addresses in its configuration. Instead, use the new `PublicAddresses` API.

Migration Guide:

```rust
    // Before:
    let (identify_config, identify_event_stream) = IdentifyConfig::new(
        "/substrate/1.0".to_string(),
        Some(user_agent),
        config.public_addresses,
    );

    // After:
    let (identify_config, identify_event_stream) =
        IdentifyConfig::new("/substrate/1.0".to_string(), Some(user_agent));
    // Public addresses must now be added using the `PublicAddresses` API:
    for address in config.public_addresses {
        if let Err(err) = public_addresses.add_address(address) {
            eprintln!("Failed to add public address: {:?}", err);
        }
    }
```

### [Dial Error and List Dial Failures Event](https://github.com/paritytech/litep2p/pull/206)

The `DialFailure` event has been enhanced with a new `DialError` enum for more precise error reporting when a dial attempt fails. Additionally, a `ListDialFailures` event has been introduced, listing all dialed addresses and their corresponding errors when multiple addresses are involved.

Other litep2p errors, such as `ParseError`, `AddressError`, and `NegotiationError`, have been refactored for improved error propagation.

### [Immediate Dial Error and Request-Response Rejection Reasons](https://github.com/paritytech/litep2p/pull/227)

This new feature paves the way for better error handling in the `litep2p` library and moves away from the overarching `litep2p::error::Error` enum.

The newly added `ImmediateDialError` enum captures errors occurring before a dial request is sent (e.g., missing peer IDs). It also enhances the `RejectReason` enum for request-response protocols, offering more detailed rejection reasons.


```rust
match error {
    RequestResponseError::Rejected(reason) => {
        match reason {
            RejectReason::ConnectionClosed => "connection-closed",
            RejectReason::DialFailed(Some(ImmediateDialError::AlreadyConnected)) => "already-connected",
            _ => "other",
        }
    }
    _ => "other",
}
```

### [Connection Limits](https://github.com/paritytech/litep2p/pull/185)

Developers can now set limits on the number of inbound and outbound connections to manage resources and optimize performance.

```rust
    // Configure connection limits for inbound and outbound established connections.
    let litep2p_config = Config::default()
        .with_connection_limits(ConnectionLimitsConfig::default()
            .max_incoming_connections(Some(3))
            .max_outgoing_connections(Some(2))
        );
```

### [Feature Flags for Optional Transports](https://github.com/paritytech/litep2p/pull/192)

The library now supports feature flags to selectively enable or disable transport protocols. By default, only the `TCP` transport is enabled. Optional transports include:

- `quic` - Enables QUIC transport.
- `websocket` - Enables WebSocket transport.
- `webrtc` - Enables WebRTC transport.

### [Configurable Keep-Alive Timeout](https://github.com/paritytech/litep2p/pull/155)

The keep-alive timeout for connections is now configurable, providing more control over connection lifecycles.

```rust
    // Set keep alive timeout for connections.
    let litep2p_config = Config::default()
        .with_keep_alive_timeout(Duration::from_secs(30));
```

Thanks for contributing to this @[Ma233](https://github.com/Ma233)!

### Added

- errors: Introduce immediate dial error and request-response rejection reasons  ([#227](https://github.com/paritytech/litep2p/pull/227))
- Expose API for `PublicAddresses`  ([#212](https://github.com/paritytech/litep2p/pull/212))
- transport: Implement `TransportService::local_peer_id()`  ([#224](https://github.com/paritytech/litep2p/pull/224))
- find_node: Optimize parallelism factor for slow to respond peers  ([#220](https://github.com/paritytech/litep2p/pull/220))
- kad: Handle `ADD_PROVIDER` & `GET_PROVIDERS` network requests  ([#213](https://github.com/paritytech/litep2p/pull/213))
- errors: Add `DialError` error and `ListDialFailures` event for better error reporting  ([#206](https://github.com/paritytech/litep2p/pull/206))
- kad: Add support for provider records to `MemoryStore`  ([#200](https://github.com/paritytech/litep2p/pull/200))
- transport: Add accept_pending/reject_pending for inbound connections and introduce inbound limits  ([#194](https://github.com/paritytech/litep2p/pull/194))
- transport/manager: Add connection limits for inbound and outbound established connections  ([#185](https://github.com/paritytech/litep2p/pull/185))
- kad: Add query id to log messages  ([#174](https://github.com/paritytech/litep2p/pull/174))

### Changed

- transport: Replace trust_dns_resolver with hickory_resolver  ([#223](https://github.com/paritytech/litep2p/pull/223))
- crypto/noise: Generate keypair only for Curve25519  ([#214](https://github.com/paritytech/litep2p/pull/214))
- transport: Allow manual setting of keep-alive timeout  ([#155](https://github.com/paritytech/litep2p/pull/155))
- kad: Update connection status of an existing bucket entry  ([#181](https://github.com/paritytech/litep2p/pull/181))
- Make transports optional  ([#192](https://github.com/paritytech/litep2p/pull/192))

### Fixed

- kad: Fix substream opening and dialing race  ([#222](https://github.com/paritytech/litep2p/pull/222))
- query-executor: Save the task waker on empty futures  ([#219](https://github.com/paritytech/litep2p/pull/219))
- substream: Use write_all instead of manually writing bytes  ([#217](https://github.com/paritytech/litep2p/pull/217))
- minor: fix tests without `websocket` feature  ([#215](https://github.com/paritytech/litep2p/pull/215))
- Fix TCP, WebSocket, QUIC leaking connection IDs in `reject()`  ([#198](https://github.com/paritytech/litep2p/pull/198))
- transport: Fix double lock and state overwrite on disconnected peers  ([#179](https://github.com/paritytech/litep2p/pull/179))
- kad: Do not update memory store on incoming `GetRecordSuccess`  ([#190](https://github.com/paritytech/litep2p/pull/190))
- transport: Reject secondary connections with different connection IDs  ([#176](https://github.com/paritytech/litep2p/pull/176))

## [0.6.2] - 2024-06-26

This is a bug fixing release. Kademlia now correctly sets and forwards publisher & ttl in the DHT records.

### Fixed

- kademlia: Preserve publisher & expiration time in DHT records ([#162](https://github.com/paritytech/litep2p/pull/162))

## [0.6.1] - 2024-06-20

This is a bug fixing and security release. curve255190-dalek has been upgraded to v4.1.3, see [dalek-cryptography/curve25519-dalek#659](https://github.com/dalek-cryptography/curve25519-dalek/pull/659) for details.

### Fixed

- kad: Set default ttl 36h for kad records ([#154](https://github.com/paritytech/litep2p/pull/154))
- chore: update ed25519-dalek to v2.1.1 ([#122](https://github.com/paritytech/litep2p/pull/122))
- Bump curve255190-dalek 4.1.2 -> 4.1.3 ([#159](https://github.com/paritytech/litep2p/pull/159))

## [0.6.0] - 2024-06-14

This release introduces breaking changes into `kad` module. The API has been extended as following:

- An event `KademliaEvent::IncomingRecord` has been added.
- New methods `KademliaHandle::store_record()` / `KademliaHandle::try_store_record()` have been introduced.

This allows implementing manual incoming DHT record validation by configuring `Kademlia` with `IncomingRecordValidationMode::Manual`.

Also, it is now possible to enable `TCP_NODELAY` on sockets.

Multiple refactorings to remove the code duplications and improve the implementation robustness have been done.

### Added

- Support manual DHT record insertion ([#135](https://github.com/paritytech/litep2p/pull/135))
- transport: Make `TCP_NODELAY` configurable ([#146](https://github.com/paritytech/litep2p/pull/146))

### Changed

- transport: Introduce common listener for tcp and websocket ([#147](https://github.com/paritytech/litep2p/pull/147))
- transport/common: Share DNS lookups between TCP and WebSocket ([#151](https://github.com/paritytech/litep2p/pull/151))

### Fixed

- ping: Make ping fault tolerant wrt outbound substreams races ([#133](https://github.com/paritytech/litep2p/pull/133))
- crypto/noise: Make noise fault tolerant ([#142](https://github.com/paritytech/litep2p/pull/142))
- protocol/notif: Fix panic on missing peer state ([#143](https://github.com/paritytech/litep2p/pull/143))
- transport: Fix erroneous handling of secondary connections ([#149](https://github.com/paritytech/litep2p/pull/149))

## [0.5.0] - 2024-05-24

This is a small release that makes the `FindNode` command a bit more robust:

- The `FindNode` command now retains the K (replication factor) best results.
- The `FindNode` command has been updated to handle errors and unexpected states without panicking.

### Added

- Add release checklist  ([#115](https://github.com/paritytech/litep2p/pull/115))

### Changed

- kad: Refactor FindNode query, keep K best results and add tests  ([#114](https://github.com/paritytech/litep2p/pull/114))

## [0.4.0] - 2024-05-23

This release introduces breaking changes to the litep2p crate, primarily affecting the `kad` module. Key updates include:

- The `GetRecord` command now exposes all peer records, not just the latest one.
- A new `RecordType` has been introduced to clearly distinguish between locally stored records and those discovered from the network.

Significant refactoring has been done to enhance the efficiency and accuracy of the `kad` module. The updates are as follows:

- The `GetRecord` command now exposes all peer records.
- The `GetRecord` command has been updated to handle errors and unexpected states without panicking.

Additionally, we've improved code coverage in the `kad` module by adding more tests.

### Added

- Re-export `multihash` & `multiaddr` types  ([#79](https://github.com/paritytech/litep2p/pull/79))
- kad: Expose all peer records of `GET_VALUE` query  ([#96](https://github.com/paritytech/litep2p/pull/96))

### Changed

- multistream_select: Remove unneeded changelog.md  ([#116](https://github.com/paritytech/litep2p/pull/116))
- kad: Refactor `GetRecord` query and add tests  ([#97](https://github.com/paritytech/litep2p/pull/97))
- kad/store: Set memory-store on an incoming record for PutRecordTo  ([#88](https://github.com/paritytech/litep2p/pull/88))
- multistream: Dialer deny multiple /multistream/1.0.0 headers  ([#61](https://github.com/paritytech/litep2p/pull/61))
- kad: Limit MemoryStore entries  ([#78](https://github.com/paritytech/litep2p/pull/78))
- Refactor WebRTC code  ([#51](https://github.com/paritytech/litep2p/pull/51))
- Revert "Bring `rustfmt.toml` in sync with polkadot-sdk (#71)"  ([#74](https://github.com/paritytech/litep2p/pull/74))
- cargo: Update str0m from 0.4.1 to 0.5.1  ([#95](https://github.com/paritytech/litep2p/pull/95))

### Fixed

- Fix clippy  ([#83](https://github.com/paritytech/litep2p/pull/83))
- crypto: Don't panic on unsupported key types  ([#84](https://github.com/paritytech/litep2p/pull/84))

## [0.3.0] - 2024-04-05

### Added

- Expose `reuse_port` option for TCP and WebSocket transports  ([#69](https://github.com/paritytech/litep2p/pull/69))
- protocol/mdns: Use `SO_REUSEPORT` for the mDNS socket  ([#68](https://github.com/paritytech/litep2p/pull/68))
- Add support for protocol/agent version  ([#64](https://github.com/paritytech/litep2p/pull/64))

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
