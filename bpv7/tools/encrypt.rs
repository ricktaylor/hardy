use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The number of the block to encrypt
    #[arg(short, long, default_value = "1", value_name = "BLOCK_NUMBER")]
    block: u64,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The security source Endpoint ID (EID) to use for signing, uses the bundle source if omitted
    #[arg(short, long)]
    source: Option<hardy_bpv7::eid::Eid>,

    /// The signing key.
    /// Can be a file path or a raw JSON string.
    #[arg(value_name = "KEY_OR_KEY_SOURCE")]
    key: String,

    /// The bundle file containing the block to sign, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        // Get the JSON content string
        let key = if self.key.trim_ascii_start().starts_with('{') {
            // Source is a raw JSON string
            self.key
        } else {
            // Source is a file path
            std::fs::read_to_string(self.key)
                .map_err(|e| anyhow::anyhow!("Failed to read key file: {e}"))?
        };

        // Parse into the enum
        let key =
            serde_json::from_str(&key).map_err(|e| anyhow::anyhow!("Failed to parse key: {e}"))?;

        let data = self.input.read_all()?;

        let bundle =
            hardy_bpv7::bundle::ParsedBundle::parse(&data, &hardy_bpv7::bpsec::key::KeySet::EMPTY)
                .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
                .bundle;

        let encryptor = hardy_bpv7::bpsec::encryptor::Encryptor::new(&bundle, &data)
            .encrypt_block(
                self.block,
                hardy_bpv7::bpsec::encryptor::Context::AES_GCM(
                    hardy_bpv7::bpsec::rfc9173::ScopeFlags::default(),
                ),
                self.source.unwrap_or(bundle.id.source.clone()),
                key,
            )
            .map_err(|e| anyhow::anyhow!("Failed to encrypt block: {e}"))?;

        let data = encryptor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?
            .1;

        self.output.write_all(&data)
    }
}
