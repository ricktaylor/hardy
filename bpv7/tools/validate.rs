use super::*;
use std::io::Read;

/// Holds the arguments for the `show` subcommand.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The list of bundle files to validate, or stdin if not supplied.
    files: Vec<String>,
}

fn parse<R: std::io::Read>(
    filename: Option<String>,
    mut input: std::io::BufReader<R>,
) -> anyhow::Result<()> {
    let mut bundle = Vec::new();
    input
        .read_to_end(&mut bundle)
        .map_err(|e| anyhow::anyhow!("Failed to read from input: {e}"))?;

    match hardy_bpv7::bundle::ValidBundle::parse(&bundle, &hardy_bpv7::bpsec::key::EmptyStore)
        .map_err(|e| {
            anyhow::anyhow!(
                "{}Failed to parse bundle: {e}",
                filename
                    .as_ref()
                    .map(|f| format!("{f}: "))
                    .unwrap_or_default()
            )
        })? {
        hardy_bpv7::bundle::ValidBundle::Valid(_, _) => Ok(()),
        hardy_bpv7::bundle::ValidBundle::Rewritten(_, _, _, non_canonical) => {
            (!non_canonical).then_some(()).ok_or(anyhow::anyhow!(
                "{}Non-canonical, but semantically valid bundle",
                filename
                    .as_ref()
                    .map(|f| format!("{f}: "))
                    .unwrap_or_default()
            ))
        }
        hardy_bpv7::bundle::ValidBundle::Invalid(_, _, error) => Err(anyhow::anyhow!(
            "{}Parser has had to guess at the content, but basically garbage: {error}",
            filename
                .as_ref()
                .map(|f| format!("{f}: "))
                .unwrap_or_default()
        )),
    }
}

pub fn exec(args: Command) -> anyhow::Result<()> {
    let mut count_failed: usize = 0;
    if args.files.is_empty() {
        if let Err(e) = parse(None, std::io::BufReader::new(std::io::stdin())) {
            eprintln!("{e}");
            count_failed = 1;
        }
    } else {
        for f in args.files {
            let input = std::io::BufReader::new(
                std::fs::File::open(&f)
                    .map_err(|e| anyhow::anyhow!("Failed to open input file: {e}"))?,
            );
            if let Err(e) = parse(Some(f), input) {
                eprintln!("{e}");
                count_failed = count_failed.saturating_add(1);
            }
        }
    }

    (count_failed == 0)
        .then_some(())
        .ok_or(anyhow::anyhow!("No files to validate"))
}
