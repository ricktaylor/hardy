fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `proto/google/rpc/status.proto` is vendored only so protoc can resolve
    // the import; the package maps onto the types `tonic-types` already
    // exports, so no second `google.rpc.Status` is generated.
    tonic_prost_build::configure()
        .protoc_arg("--experimental_allow_proto3_optional")
        .extern_path(".google.rpc", "::tonic_types::pb")
        .compile_protos(&["tvr.proto"], &[".", "proto"])?;
    Ok(())
}
