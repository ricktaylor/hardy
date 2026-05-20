use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Extract the data from a block in a bundle",
    long_about = "Extract the data from a block in a bundle.\n\n\
        Extracts and outputs the raw data from a specific block. By default extracts \
        block 1 (the payload block). If the block is encrypted and keys are provided, \
        it will be automatically decrypted before extraction."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// The number of the block to extract
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// Path to the location to write the extracted data to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file from which to extract a block, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        // Structural parse + keyed BPSec validation in one pass; gives us
        // the bcb_ops map we need for the per-block decrypt below.
        let parse::Parsed {
            data,
            bundle,
            bcbs: bcb_ops,
            ..
        } = parse_with_keys(data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?;

        let payload =
            hardy_bpv7::bpsec::block_data(self.block, &bundle.blocks, &data, &bcb_ops, &key_store)
                .map_err(|e| anyhow::anyhow!("Failed to decrypt block: {e}"))?;

        self.output.write_all(payload.as_ref())
    }
}
