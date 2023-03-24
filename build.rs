fn main() {
    prost_build::compile_protos(
        &["src/schema/keys.proto", "src/schema/types.proto"],
        &["src"],
    )
    .unwrap();
}
