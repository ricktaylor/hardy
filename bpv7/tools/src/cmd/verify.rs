use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Verify the integrity signature of a block",
    long_about = "Verify the integrity signature of a block.\n\n\
        Verifies that the specified block's BIB signature is valid using the \
        provided keys. Returns success (exit code 0) if verification passes, \
        or an error if the block is not protected or verification fails."
)]
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

        // `parse_with_keys` runs Section C7 with the keys, so a
        // successful return means every BIB whose key we have verified.
        // We only need to report whether THIS block was BIB-covered.
        let parse::Parsed { bundle, .. } = parse_with_keys(data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to verify block: {e}"))?;

        let target = bundle
            .blocks
            .get(&self.block)
            .ok_or_else(|| anyhow::anyhow!("Bundle has no block {}", self.block))?;
        match target.bib {
            hardy_bpv7::block::BibCoverage::Some(_) => Ok(()),
            hardy_bpv7::block::BibCoverage::None => Err(anyhow::anyhow!(
                "Block {} is not protected by a BIB",
                self.block
            )),
            hardy_bpv7::block::BibCoverage::Maybe => Err(anyhow::anyhow!(
                "Block {} is covered by a BIB whose body could not be decrypted (NoKey)",
                self.block
            )),
        }
    }
}
