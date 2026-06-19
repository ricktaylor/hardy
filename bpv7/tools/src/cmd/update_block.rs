use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Update an existing block in a bundle",
    long_about = "Update an existing block in a bundle.\n\n\
        Modifies an existing block's payload, flags, or CRC type. At least one \
        update option must be specified. The block is identified by its block number.\n\n\
        If the block is a security target of a BIB or BCB, it will be automatically \
        removed from those target lists (since the signature/encryption would be \
        invalid after modification). If the BIB itself is encrypted, use \
        remove-integrity first."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The number of the block to update
    #[arg(short = 'n', long = "block-number", value_name = "BLOCK_NUMBER")]
    block_number: u64,

    /// Block payload from command line
    #[arg(short, long, conflicts_with = "payload_file")]
    payload: Option<String>,

    /// Path to file containing block payload, '-' for stdin
    #[arg(long = "payload-file", conflicts_with = "payload")]
    payload_file: Option<io::Input>,

    /// Block processing flags (comma-separated)
    #[arg(short, long, value_delimiter = ',')]
    flags: Vec<flags::ArgBlockFlags>,

    /// CRC type for the block
    #[arg(short, long)]
    crc_type: Option<flags::ArgCrcType>,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file containing the block to update, '-' to use stdin
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

        let editor = hardy_bpv7::editor::Editor::new(&raw, &data);

        let mut block_builder = editor
            .update_block(self.block_number)
            .map_err(|(_, e)| anyhow::anyhow!("Failed to update block: {e}"))?;

        // Update payload if provided
        if let Some(payload_str) = &self.payload {
            block_builder = block_builder.with_data(payload_str.as_bytes().to_vec().into());
        } else if let Some(input) = &self.payload_file {
            // BlockBuilder::with_data wants Cow<[u8]>; read_all returns Bytes,
            // so copy into an owned Vec for Cow::Owned.
            block_builder = block_builder.with_data(input.read_all()?.to_vec().into());
        }

        // Update flags if provided
        if !self.flags.is_empty() {
            block_builder =
                block_builder.with_flags(flags::ArgBlockFlags::to_block_flags(&self.flags));
        }

        // Update CRC type if provided
        if let Some(crc_type) = self.crc_type {
            block_builder = block_builder.with_crc_type(crc_type.into());
        }

        let editor = block_builder.rebuild();

        let chunks = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        let out = hardy_bpv7::editor::Chunk::flatten_bytes(chunks, data);
        self.output.write_all(&out)
    }
}
