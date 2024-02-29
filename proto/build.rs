fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("google/rpc/status.proto")?;
    tonic_build::compile_protos("bpa.proto")?;
    Ok(())
}
