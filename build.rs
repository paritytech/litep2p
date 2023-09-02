fn main() {
    prost_build::compile_protos(
        &[
            "src/schema/keys.proto",
            "src/schema/noise.proto",
            "src/schema/webrtc.proto",
            "src/protocol/libp2p/schema/identify.proto",
            "src/protocol/libp2p/schema/kademlia.proto",
            "src/protocol/libp2p/schema/bitswap.proto",
        ],
        &["src"],
    )
    .unwrap();
}
