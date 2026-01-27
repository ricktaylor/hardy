use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Decrypt a block and remove it from BCB protection",
    long_about = "Decrypt a block and remove it from BCB protection.\n\n\
        This command removes the specified block from the BCB's security target list \
        and restores the block's plaintext data. The BCB block itself is only removed \
        from the bundle if it has no remaining security targets.\n\n\
        See RFC 9172 Section 3.4 for details on security targets."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The number of the block to remove encryption from
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file from which to remove encryption, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        let bundle = hardy_bpv7::bundle::ParsedBundle::parse_with_keys(&data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
            .bundle;

        let editor = hardy_bpv7::editor::Editor::new(&bundle, &data)
            .remove_encryption(self.block, &key_store)
            .map_err(|(_, e)| anyhow::anyhow!("Failed to remove encryption: {e}"))?;

        let data = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        self.output.write_all(&data)
    }
}
