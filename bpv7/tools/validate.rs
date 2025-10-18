use super::*;
use std::io::Read;

/// Holds the arguments for the `show` subcommand.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    /// The list of bundle files to validate, or stdin if not supplied.
    files: Vec<String>,
}

fn parse<R: std::io::Read>(filename: Option<String>, mut input: std::io::BufReader<R>) -> bool {
    let mut bundle = Vec::new();
    input
        .read_to_end(&mut bundle)
        .expect("Failed to read from input");

    match hardy_bpv7::bundle::ValidBundle::parse(&bundle, &hardy_bpv7::bpsec::key::EmptyStore) {
        Ok(hardy_bpv7::bundle::ValidBundle::Valid(_, _)) => true,
        Ok(hardy_bpv7::bundle::ValidBundle::Rewritten(_, _, _, non_canonical)) => {
            if non_canonical {
                println!(
                    "{}Non-canonical, but semantically valid bundle",
                    filename.map(|f| format!("{f}: ")).unwrap_or_default()
                );
                false
            } else {
                true
            }
        }
        Ok(hardy_bpv7::bundle::ValidBundle::Invalid(_, _, error)) => {
            eprintln!(
                "{}Parser has had to guess at the content, but basically garbage: {error}",
                filename.map(|f| format!("{f}: ")).unwrap_or_default()
            );
            false
        }
        Err(e) => {
            eprintln!(
                "{}Failed to parse bundle: {e}",
                filename.map(|f| format!("{f}: ")).unwrap_or_default()
            );
            false
        }
    }
}

pub fn exec(args: Command) -> ExitCode {
    let mut count_failed: usize = 0;
    if args.files.is_empty() {
        if !parse(None, std::io::BufReader::new(std::io::stdin())) {
            count_failed = 1;
        }
    } else {
        for f in args.files {
            let input = std::io::BufReader::new(
                std::fs::File::open(&f).expect("Failed to open input file"),
            );
            if !parse(Some(f), input) {
                count_failed = count_failed.saturating_add(1);
            }
        }
    }

    if count_failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
