fn main() {
    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile(&["proto/filesearch.proto"], &["proto"])
        .unwrap_or_else(|e| panic!("protoc failed: {e}"));
}
