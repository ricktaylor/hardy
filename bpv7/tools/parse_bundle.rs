use clap::Parser;
use std::{io::Read, process::ExitCode};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The list of additional arguments/files
    /// Clap automatically collects all positional arguments here.
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

fn main() -> ExitCode {
    let args = Args::parse();
    let mut count_failed: usize = 0;
    if args.files.is_empty() {
        if !parse(None, std::io::BufReader::new(std::io::stdin())) {
            count_failed = count_failed.saturating_add(1);
        }
    } else {
        for f in args.files {
            if !parse(
                Some(f.clone()),
                std::io::BufReader::new(std::fs::File::open(f).expect("Failed to open input file")),
            ) {
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
