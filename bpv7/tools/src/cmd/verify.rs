use super::*;
use hardy_bpv7::bpsec::key::KeySet;
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
        let key_store: KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        // `parse_with_keys` runs the structural parse and keyed verification,
        // but soft-skips any BIB whose key is absent, and the accepted copy
        // leaves encrypted what the keys cannot decrypt — so neither `Ok`
        // means this block's signature was checked. `verify_block` does the
        // real check over the accepted copy, where every BIB the keys decrypt
        // is plaintext: it calls `op.verify` and surfaces a missing key
        // (NoKey/Maybe) or a failed signature as an error, so `bundle verify`
        // can't pass on the structural `BibCoverage` stamp alone.
        let parsed = parse_with_keys(data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to verify block: {e}"))?;
        let accepted = accepted_copy(&parsed, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to verify block: {e}"))?;

        if verify_block(
            self.block,
            &accepted.bundle.blocks,
            &accepted.data,
            &accepted.bibs,
            &key_store,
        )
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
