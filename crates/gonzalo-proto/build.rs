//! Generate gRPC client+server stubs from `proto/gonzalo.proto`. We supply
//! a vendored `protoc` so no system protobuf install is required.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protoc = protoc_bin_vendored::protoc_bin_path()?;
    // SAFETY: build scripts are single-threaded; setting PROTOC here is the
    // documented way to point prost-build at the vendored compiler.
    #[allow(unsafe_code)]
    unsafe {
        std::env::set_var("PROTOC", protoc);
    }
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&["proto/gonzalo.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/gonzalo.proto");
    Ok(())
}
