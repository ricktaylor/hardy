use super::*;

/// Holds the arguments for the `show` subcommand.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    // Use #[command(flatten)] to include the --key argument
    #[command(flatten)]
    key: keys::KeyLoaderArgs,

    /// The number of the block to verify
    #[arg(short, long, default_value = "1")]
    block: u64,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, default_value = "")]
    output: io::Output,

    /// The security source to use for signing
    source: hardy_bpv7::eid::Eid,

    /// The bundle file in which to verify a block, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let Some(key) = self.key.try_into()? else {
            return Err(anyhow::anyhow!("A key must be provided for signing"));
        };

        let data = self.input.read_all()?;

        let bundle =
            hardy_bpv7::bundle::ParsedBundle::parse(&data, &hardy_bpv7::bpsec::key::KeySet::EMPTY)
                .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
                .bundle;

        let signer = hardy_bpv7::bpsec::signer::Signer::new(&bundle, &data)
            .sign_block(
                self.block,
                hardy_bpv7::bpsec::signer::Context::HMAC_SHA2(
                    hardy_bpv7::bpsec::rfc9173::ScopeFlags::default(),
                ),
                self.source,
                key,
            )
            .map_err(|e| anyhow::anyhow!("Failed to sign block: {e}"))?;

        let data = signer
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?
            .1;

        self.output.write_all(&data)
    }
}
