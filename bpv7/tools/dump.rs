use super::*;

/// Holds the arguments for the `show` subcommand.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    // Use #[command(flatten)] to include the --key argument
    #[command(flatten)]
    key_args: keys::KeyLoaderArgs,

    #[arg(short, long, long_help = "Pretty-print the output")]
    pretty: bool,

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

    let bundle = args
        .input
        .read_all()
        .map_err(|e| anyhow::anyhow!("Failed to read input from {}: {e}", args.input.filepath()))?;

    let bundle = match hardy_bpv7::bundle::ValidBundle::parse(&bundle, &key_store)
        .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?
    {
        hardy_bpv7::bundle::ValidBundle::Valid(bundle, _) => bundle,
        hardy_bpv7::bundle::ValidBundle::Rewritten(bundle, _, _, _) => {
            eprintln!(
                "{}: Non-canonical, but semantically valid bundle",
                args.input.filepath()
            );
            bundle
        }
        hardy_bpv7::bundle::ValidBundle::Invalid(bundle, _, error) => {
            eprint!(
                "{}: Parser has had to guess at the content, but basically garbage: {error}",
                args.input.filepath()
            );
            bundle
        }
    };

    let mut json = if args.pretty {
        serde_json::to_string_pretty(&bundle)
    } else {
        serde_json::to_string(&bundle)
    }
    .map_err(|e| anyhow::anyhow!("Failed to serialize bundle: {e}"))?;
    json.push('\n');

    let mut output = io::Output::new(args.output);
    output
        .write_all(json.as_bytes())
        .map_err(|e| anyhow::anyhow!("Failed to write output to {}: {e}", output.filepath()))
}
