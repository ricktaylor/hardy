use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    // Use #[command(flatten)] to include the --key argument
    #[command(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The number of the block to extract
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// Path to the location to write the extracted data to, or stdout if not supplied
    #[arg(short, long, required = false)]
    output: io::Output,

    /// The bundle file from which to extract a block, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        let bundle = hardy_bpv7::bundle::ParsedBundle::parse(&data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
            .bundle;

        match bundle
            .decrypt_block(self.block, &data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to decrypt block: {e}"))?
        {
            hardy_bpv7::bundle::Payload::Range(range) => self.output.write_all(&data[range]),
            hardy_bpv7::bundle::Payload::Owned(data) => self.output.write_all(&data),
        }
    }
}
