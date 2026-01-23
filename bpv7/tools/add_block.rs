use super::*;
use hardy_bpv7::block;
use std::str::FromStr;

#[derive(Debug, Clone)]
enum BlockTypeArg {
    BundleAge,
    HopCount,
    PreviousNode,
    BlockIntegrity,
    BlockSecurity,
    Numeric(u64),
}

impl FromStr for BlockTypeArg {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "bundle-age" | "age" => Ok(Self::BundleAge),
            "hop-count" | "hop" => Ok(Self::HopCount),
            "previous-node" | "prev" => Ok(Self::PreviousNode),
            "block-integrity" | "bib" => Ok(Self::BlockIntegrity),
            "block-security" | "bcb" => Ok(Self::BlockSecurity),
            _ => s
                .parse::<u64>()
                .map(Self::Numeric)
                .map_err(|_| anyhow::anyhow!("Invalid block type: {}", s)),
        }
    }
}

impl From<BlockTypeArg> for block::Type {
    fn from(value: BlockTypeArg) -> Self {
        match value {
            BlockTypeArg::BundleAge => block::Type::BundleAge,
            BlockTypeArg::HopCount => block::Type::HopCount,
            BlockTypeArg::PreviousNode => block::Type::PreviousNode,
            BlockTypeArg::BlockIntegrity => block::Type::BlockIntegrity,
            BlockTypeArg::BlockSecurity => block::Type::BlockSecurity,
            BlockTypeArg::Numeric(n) => block::Type::Unrecognised(n),
        }
    }
}

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// Block type to add (bundle-age, hop-count, previous-node, or numeric type code)
    #[arg(short = 't', long = "type", value_name = "BLOCK_TYPE")]
    block_type: BlockTypeArg,

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

    /// Force adding block even if one of the same type already exists
    #[arg(long)]
    force: bool,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file to add the block to, '-' to use stdin
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let data = self.input.read_all()?;

        let bundle =
            hardy_bpv7::bundle::ParsedBundle::parse(&data, &hardy_bpv7::bpsec::key::KeySet::EMPTY)
                .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
                .bundle;

        // Get block payload
        let block_data = if let Some(payload_str) = &self.payload {
            // Treat as raw bytes
            payload_str.as_bytes().to_vec()
        } else if let Some(input) = &self.payload_file {
            input.read_all()?
        } else {
            return Err(anyhow::anyhow!(
                "Either --payload or --payload-file must be provided"
            ));
        };

        let block_type: block::Type = self.block_type.into();

        let editor = hardy_bpv7::editor::Editor::new(&bundle, &data);

        // Try to add the block, if it fails due to duplicate and --force is set, replace it
        let block_builder = match editor.push_block(block_type) {
            Ok(builder) => builder,
            Err((editor, hardy_bpv7::editor::Error::IllegalDuplicate(_))) if self.force => {
                // User explicitly requested --force, so replace the existing block
                editor
                    .insert_block(block_type)
                    .map_err(|(_, e)| anyhow::anyhow!("Failed to insert block: {e}"))?
            }
            Err((_, e)) => return Err(anyhow::anyhow!("Failed to add block: {e}")),
        };

        // Set flags if provided
        let block_builder = if !self.flags.is_empty() {
            block_builder.with_flags(flags::ArgBlockFlags::to_block_flags(&self.flags))
        } else {
            block_builder
        };

        // Set CRC type if provided
        let block_builder = if let Some(crc_type) = self.crc_type {
            block_builder.with_crc_type(crc_type.into())
        } else {
            block_builder
        };

        // Set block data and rebuild
        let editor = block_builder.with_data(block_data.into()).rebuild();

        let data = editor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        self.output.write_all(&data)
    }
}
