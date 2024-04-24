fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("cla.proto")?;
    tonic_build::compile_protos("application.proto")?;
    Ok(())
}
