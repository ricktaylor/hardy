use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
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
        let data = self.input.read_all()?;

        let bundle = hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bundle::no_keys)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
            .bundle;

        let editor = hardy_bpv7::editor::Editor::new(&bundle, &data);

        let mut block_builder = editor
            .update_block(self.block_number)
            .map_err(|(_, e)| anyhow::anyhow!("Failed to update block: {e}"))?;

        // Update payload if provided
        if let Some(payload_str) = &self.payload {
            block_builder = block_builder.with_data(payload_str.as_bytes().to_vec().into());
        } else if let Some(input) = &self.payload_file {
            block_builder = block_builder.with_data(input.read_all()?.into());
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

        let data = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        self.output.write_all(&data)
    }
}
