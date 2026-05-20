use super::*;

#[derive(Parser, Debug)]
#[command(
    about = "Rewrite a bundle, removing unsupported blocks and canonicalizing",
    long_about = "Rewrite a bundle, removing unsupported blocks and canonicalizing.\n\n\
        Parses the input bundle and rewrites it in canonical form. Unsupported or \
        malformed extension blocks are removed according to their block processing \
        flags. This is useful for cleaning up bundles received from other implementations."
)]
pub struct Command {
    #[clap(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, required = false, default_value = "")]
    output: io::Output,

    /// The bundle file to dump, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        let data = match super::full_rewrite(data.clone(), &key_store) {
            Ok(None) => data,
            Ok(Some(chunks)) => hardy_bpv7::editor::Chunk::flatten_bytes(chunks, data),
            Err(error) => {
                return Err(anyhow::anyhow!("Failed to parse bundle: {error}"));
            }
        };

        self.output.write_all(&data)
    }
}
