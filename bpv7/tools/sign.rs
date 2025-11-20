use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The number of the block to sign
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The security source Endpoint ID (EID) to use for signing, uses the bundle source if omitted
    #[arg(short, long)]
    source: Option<hardy_bpv7::eid::Eid>,

    #[clap(flatten)]
    key_input: keys::KeyInput,

    /// The bundle file containing the block to sign, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key = self.key_input.try_into()?;
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
                self.source.unwrap_or(bundle.id.source.clone()),
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
