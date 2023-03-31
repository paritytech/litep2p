fn main() {
    prost_build::compile_protos(
        &[
            "src/schema/keys.proto",
            "src/schema/noise.proto",
            "src/protocol/libp2p/schema/identify.proto",
        ],
        &["src"],
    )
    .unwrap();
}
