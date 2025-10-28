use super::*;

/// Holds the arguments for the `show` subcommand.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    // Use #[command(flatten)] to include the --key argument
    #[command(flatten)]
    key_args: keys::KeyLoaderArgs,

    #[arg(
        short,
        long,
        long_help = "Path to the location to write the bundle to, or stdout if not supplied"
    )]
    output: Option<PathBuf>,

    /// The file to dump, '-' to use stdin.
    input: io::Input,
}

pub fn exec(args: Command) -> anyhow::Result<()> {
    let key_store: hardy_bpv7::bpsec::key::KeySet = args.key_args.try_into()?;

    let data = args.input.read_all()?;

    let data = match hardy_bpv7::bundle::RewrittenBundle::parse(&data, &key_store)
        .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
    {
        hardy_bpv7::bundle::RewrittenBundle::Valid { .. } => data,
        hardy_bpv7::bundle::RewrittenBundle::Rewritten { new_data, .. } => new_data.into(),
        hardy_bpv7::bundle::RewrittenBundle::Invalid { error, .. } => {
            return Err(anyhow::anyhow!("Failed to parse bundle: {error}"));
        }
    };

    io::Output::new(args.output).write_all(&data)
}
