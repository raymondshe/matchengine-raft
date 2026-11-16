fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/raft.proto");

    // Do not require a system protoc: the vendored binary keeps builds reproducible
    // on macOS and Linux CI alike.
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }

    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&["proto/raft.proto"], &["proto"])?;

    Ok(())
}
