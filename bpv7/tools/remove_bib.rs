use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The number of the block to remove integrity protection from
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file from which to remove the BIB, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        let bundle = hardy_bpv7::bundle::ParsedBundle::parse(&data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
            .bundle;

        let editor = hardy_bpv7::editor::Editor::new(&bundle, &data)
            .remove_integrity(self.block)
            .map_err(|(_, e)| anyhow::anyhow!("Failed to remove integrity check: {e}"))?;

        let data = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        self.output.write_all(&data)
    }
}
