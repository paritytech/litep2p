fn main() {
    prost_build::compile_protos(
        &["src/schema/keys.proto", "src/schema/noise.proto"],
        &["src"],
    )
    .unwrap();
}
