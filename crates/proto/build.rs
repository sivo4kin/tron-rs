//! Compile the vendored Tron protobuf schema into Rust.
//!
//! The protos live under `protos/` (vendored from java-tron's
//! `protocol/src/main/protos`). All Tron messages share the `protocol` package.
//! We generate both gRPC client and server (used by the RPC crate in P4).

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "protos/api/api.proto",
        "protos/core/Discover.proto",
        "protos/core/TronInventoryItems.proto",
    ];
    let includes = ["protos"];

    tonic_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_protos(&protos, &includes)?;

    for p in protos {
        println!("cargo:rerun-if-changed={p}");
    }
    println!("cargo:rerun-if-changed=protos");
    Ok(())
}
