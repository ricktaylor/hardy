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

        // `parse_with_keys` runs the structural parse and keyed BIB verification,
        // but soft-skips any BIB whose key is absent — so its `Ok` does NOT mean
        // this block's signature was checked. `verify_block` does the real check:
        // it calls `op.verify` and surfaces a missing key (NoKey/Maybe) or a
        // failed signature as an error, so `bundle verify` can't pass on the
        // structural `BibCoverage` stamp alone.
        let parse::Parsed {
            data,
            bundle,
            bibs: bib_ops,
            ..
        } = parse_with_keys(data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to verify block: {e}"))?;

        if verify_block(self.block, &bundle.blocks, &data, &bib_ops, &key_store)
            .map_err(|e| anyhow::anyhow!("Cannot verify block {}: {e}", self.block))?
        {
            Ok(())
        } else {
            Err(anyhow::anyhow!(
                "Block {} is not protected by a BIB",
                self.block
            ))
        }
    }
}
