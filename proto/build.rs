fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure().bytes(".").compile_protos(
        &[
            "proto/application.proto",
            "proto/service.proto",
            "proto/cla.proto",
            "proto/routing.proto",
        ],
        &["proto"],
    )?;
    Ok(())
}
