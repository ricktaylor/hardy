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

    let bundle = args.input.read_all()?;

    let p = hardy_bpv7::bundle::ParsedBundle::parse(&bundle, &key_store)
        .map_err(|e| anyhow::anyhow!("Failed to parse bundle: {e}"))?;

    if p.non_canonical {
        eprintln!(
            "{}: Non-canonical, but semantically valid bundle",
            args.input.filepath()
        );
    }

    let mut json = if args.pretty {
        serde_json::to_string_pretty(&p.bundle)
    } else {
        serde_json::to_string(&p.bundle)
    }
    .map_err(|e| anyhow::anyhow!("Failed to serialize bundle: {e}"))?;
    json.push('\n');

    io::Output::new(args.output).write_all(json.as_bytes())
}
