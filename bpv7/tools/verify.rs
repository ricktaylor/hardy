use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The number of the block to verify
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// The bundle file in which to verify a block, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        let bundle = hardy_bpv7::bundle::ParsedBundle::parse(&data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
            .bundle;

        bundle
            .verify_block(self.block, &data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to verify block: {e}"))
    }
}
