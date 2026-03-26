fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .bytes(".")
        .protoc_arg("--experimental_allow_proto3_optional")
        .compile_protos(
            &[
                "cla.proto",
                "service.proto",
                "routing.proto",
                "google/rpc/status.proto",
            ],
            &["."],
        )?;
    Ok(())
}
