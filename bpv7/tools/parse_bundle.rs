use clap::Parser;
use std::{io::Read, process::ExitCode};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The list of additional arguments/files
    /// Clap automatically collects all positional arguments here.
    files: Vec<String>,
}

struct NoKeys;

impl hardy_bpv7::bpsec::key::KeyStore for NoKeys {
    fn decrypt_keys<'a>(
        &'a self,
        _source: &hardy_bpv7::eid::Eid,
        _operation: &[hardy_bpv7::bpsec::key::Operation],
    ) -> impl Iterator<Item = &'a hardy_bpv7::bpsec::key::Key> {
        None.into_iter()
    }
}

fn parse<R: std::io::Read>(mut input: std::io::BufReader<R>) -> bool {
    let mut bundle = Vec::new();
    input
        .read_to_end(&mut bundle)
        .expect("Failed to read from input");

    match hardy_bpv7::bundle::ValidBundle::parse(&bundle, &NoKeys) {
        Ok(hardy_bpv7::bundle::ValidBundle::Valid(_, _)) => {
            println!("Ok");
            true
        }
        Ok(hardy_bpv7::bundle::ValidBundle::Rewritten(_, _, _, non_canonical)) => {
            if non_canonical {
                println!("Non-canonical, but semantically valid bundle");
                false
            } else {
                println!("Ok");
                true
            }
        }
        Ok(hardy_bpv7::bundle::ValidBundle::Invalid(_, _, error)) => {
            eprintln!("Parser has had to guess at the content, but basically garbage: {error}");
            false
        }
        Err(e) => {
            eprintln!("Failed to parse bundle: {e}");
            false
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    let mut count_failed: usize = 0;
    if args.files.is_empty() {
        if !parse(std::io::BufReader::new(std::io::stdin())) {
            count_failed = count_failed.saturating_add(1);
        }
    } else {
        for f in args.files {
            print!("Reading {f}... ");
            if !parse(std::io::BufReader::new(
                std::fs::File::open(f).expect("Failed to open input file"),
            )) {
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
