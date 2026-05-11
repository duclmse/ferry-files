fn main() {
    // Use the bundled protoc binary — eliminates the need for protoc on PATH
    // on Windows and other platforms that don't package it by default.
    let protoc = protoc_bin_vendored::protoc_bin_path()
        .expect("protoc-bin-vendored: could not locate bundled protoc binary");
    std::env::set_var("PROTOC", protoc);

    tonic_build::configure()
        .build_server(true)
        .build_client(false)
        .compile(&["proto/filesearch.proto"], &["proto"])
        .unwrap_or_else(|e| panic!("tonic_build failed: {e}"));
}
