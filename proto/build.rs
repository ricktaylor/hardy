use std::path::Path;

// Clone of tonic_build::compile_protos() to add --experimental_allow_proto3_optional
fn compile_proto(proto: impl AsRef<Path>) -> std::io::Result<()> {
    let proto_path: &Path = proto.as_ref();

    // directory the main .proto file resides in
    let proto_dir = proto_path
        .parent()
        .expect("proto file should reside in a directory");

    tonic_prost_build::configure()
        .bytes(".")
        .protoc_arg("--experimental_allow_proto3_optional") // for older systems
        .compile_protos(&[proto_path], &[proto_dir])
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    compile_proto("cla.proto")?;
    compile_proto("service.proto")?;
    compile_proto("google/rpc/status.proto")?;

    Ok(())
}
