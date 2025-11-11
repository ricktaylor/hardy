use super::*;

#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    // Use #[command(flatten)] to include the --key argument
    #[command(flatten)]
    key_args: keys::KeySetLoaderArgs,

    /// Path to the location to write the bundle to, or stdout if not supplied
    #[arg(short, long, default_value = "")]
    output: io::Output,

    /// The bundle file to dump, '-' to use stdin.
    input: io::Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let key_store: hardy_bpv7::bpsec::key::KeySet = self.key_args.try_into()?;

        let data = self.input.read_all()?;

        let data = match hardy_bpv7::bundle::RewrittenBundle::parse(&data, &key_store)
            .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
        {
            hardy_bpv7::bundle::RewrittenBundle::Valid { .. } => data,
            hardy_bpv7::bundle::RewrittenBundle::Rewritten { new_data, .. } => new_data.into(),
            hardy_bpv7::bundle::RewrittenBundle::Invalid { error, .. } => {
                return Err(anyhow::anyhow!("Failed to parse bundle: {error}"));
            }
        };

        self.output.write_all(&data)
    }
}
