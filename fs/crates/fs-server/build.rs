fn main() {
    #[cfg(feature = "grpc")]
    {
        // Use the bundled protoc binary - eliminates the need for protoc on PATH
        // on Windows and other platforms that don't package it by default.
        let protoc = protoc_bin_vendored::protoc_bin_path()
            .expect("protoc-bin-vendored: could not locate bundled protoc binary");
        std::env::set_var("PROTOC", protoc);

        let out_dir = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());

        tonic_prost_build::configure()
            .build_server(true)
            .build_client(false)
            // Emit a file descriptor set so tonic-reflection can enumerate RPCs.
            .file_descriptor_set_path(out_dir.join("fs_descriptor.bin"))
            .compile_protos(&["proto/fs.proto"], &["proto"])
            .unwrap_or_else(|e| panic!("tonic_prost_build failed: {e}"));
    }
}
