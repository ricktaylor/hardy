use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Remove a block from BIB protection",
    long_about = "Remove a block from BIB protection.\n\n\
        This command removes the specified block from the BIB's security target list, \
        discarding its integrity signature. The BIB block itself is only removed from \
        the bundle if it has no remaining security targets.\n\n\
        See RFC 9172 Section 3.4 for details on security targets."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The number of the block to remove integrity protection from
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file from which to remove integrity protection, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        // Structural parse + keyed BPSec validation in one pass
        // (see `cmd::parse_with_keys` for the stage list).
        let parse::Parsed {
            data, bundle: raw, ..
        } = parse_with_keys(data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?;

        use hardy_bpv7::bpsec::edit::BPSecEditor;
        let editor = hardy_bpv7::editor::Editor::new(&raw, &data)
            .remove_integrity(self.block)
            .map_err(|(_, e)| anyhow::anyhow!("Failed to remove integrity check: {e}"))?;

        let chunks = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        let out = hardy_bpv7::editor::Chunk::flatten_bytes(chunks, data);
        self.output.write_all(&out)
    }
}
