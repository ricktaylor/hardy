fn main() -> Result<(), Box<dyn std::error::Error>> {
    // The shared and registry packages are generated on their own, because the
    // pass below maps `hardy.common.v1` to the module that includes it and so
    // emits nothing for it.
    tonic_prost_build::configure()
        .bytes(".")
        .compile_protos(&["proto/common.proto"], &["proto"])?;

    // Each API is included in its own module, one level below the crate root, so
    // a generated relative path to another package would escape the crate.
    // A message carrying the session token gets a hand-written `Debug` in
    // `lib.rs` that prints the token's length, never its bytes.
    tonic_prost_build::configure()
        .bytes(".")
        .extern_path(".hardy.common.v1", "crate::common")
        .skip_debug([
            ".hardy.application.v1.Registration",
            ".hardy.application.v1.SendMetadata",
            ".hardy.application.v1.ReceiveMetadata",
            ".hardy.service.v1.Registration",
            ".hardy.service.v1.SendMetadata",
            ".hardy.service.v1.ReceiveMetadata",
            ".hardy.cla.v1.Registration",
            ".hardy.cla.v1.DispatchMetadata",
            ".hardy.cla.v1.ForwardMetadata",
            ".hardy.cla.v1.AddPeerRequest",
            ".hardy.cla.v1.RemovePeerRequest",
            ".hardy.cla.v1.ReportTransferOutcomeRequest",
            ".hardy.routing.v1.Registration",
            ".hardy.routing.v1.AddRouteRequest",
            ".hardy.routing.v1.RemoveRouteRequest",
        ])
        .compile_protos(
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
