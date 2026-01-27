use super::*;

mod rfc9173 {
    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
    pub enum ArgFlags {
        /// Include all flags (default)
        All,

        /// Clear all flags
        None,

        /// Include Primary Block
        #[value(name = "primary")]
        IncludePrimaryBlock,

        /// Include Target Header
        #[value(name = "target")]
        IncludeTargetHeader,

        /// Include Security Source Header
        #[value(name = "source")]
        IncludeSecurityHeader,
    }

    impl ArgFlags {
        pub fn to_scope_flags(args: &[ArgFlags]) -> hardy_bpv7::bpsec::rfc9173::ScopeFlags {
            // If no flags specified, use Default (all true)
            if args.is_empty() {
                return hardy_bpv7::bpsec::rfc9173::ScopeFlags::default();
            }

            // If individual flags specified, start from NONE and enable only specified ones
            let mut flags = hardy_bpv7::bpsec::rfc9173::ScopeFlags::NONE;

            for arg in args {
                match arg {
                    ArgFlags::None => flags = hardy_bpv7::bpsec::rfc9173::ScopeFlags::NONE,
                    ArgFlags::All => {
                        flags.include_primary_block = true;
                        flags.include_target_header = true;
                        flags.include_security_header = true;
                    }
                    ArgFlags::IncludePrimaryBlock => flags.include_primary_block = true,
                    ArgFlags::IncludeTargetHeader => flags.include_target_header = true,
                    ArgFlags::IncludeSecurityHeader => flags.include_security_header = true,
                }
            }
            flags
        }
    }
}

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

    #[clap(flatten)]
    key_input: keys::KeyInput,

    /// One or more scope flags, separated by ','
    #[arg(short, long, value_delimiter = ',')]
    flags: Vec<rfc9173::ArgFlags>,

    /// The bundle file containing the block to sign, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key = self.key_input.try_into()?;
        let data = self.input.read_all()?;

        let bundle = hardy_bpv7::bundle::ParsedBundle::parse(&data, hardy_bpv7::bundle::no_keys)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
            .bundle;

        let encryptor = hardy_bpv7::bpsec::encryptor::Encryptor::new(&bundle, &data)
            .encrypt_block(
                self.block,
                hardy_bpv7::bpsec::encryptor::Context::AES_GCM(rfc9173::ArgFlags::to_scope_flags(
                    &self.flags,
                )),
                self.source.unwrap_or(bundle.id.source.clone()),
                &key,
            )
            .map_err(|(_, e)| anyhow::anyhow!("Failed to encrypt block: {e}"))?;

        let data = encryptor
            .rebuild()
            .map_err(|e| anyhow::anyhow!("Failed to rebuild bundle: {e}"))?;

        self.output.write_all(&data)
    }
}
