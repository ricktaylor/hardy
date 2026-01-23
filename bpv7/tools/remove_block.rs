use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The number of the block to remove
    #[arg(short = 'n', long = "block-number", value_name = "BLOCK_NUMBER")]
    block_number: u64,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file from which to remove the block, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let data = self.input.read_all()?;

        let bundle =
            hardy_bpv7::bundle::ParsedBundle::parse(&data, &hardy_bpv7::bpsec::key::KeySet::EMPTY)
                .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
                .bundle;

        let editor = hardy_bpv7::editor::Editor::new(&bundle, &data)
            .remove_block(self.block_number)
            .map_err(|(_, e)| {
                anyhow::anyhow!("Failed to remove block {}: {e}", self.block_number)
            })?;

        let data = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        self.output.write_all(&data)
    }
}
