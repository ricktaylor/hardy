use super::*;
use hardy_bpv7::eid::Eid;
use std::io::{BufRead, BufWriter, Write};

/// Holds the arguments for the `create` subcommand.
#[derive(Parser, Debug)]
#[command(about, long_about = None)]
pub struct Command {
    #[arg(short, long, long_help = "The source Endpoint ID (EID) of the bundle")]
    source: Eid,

    #[arg(
        short,
        long,
        long_help = "The destination Endpoint ID (EID) of the bundle"
    )]
    destination: Eid,

    #[arg(
        short,
        long = "report-to",
        long_help = "The optional 'Report To' Endpoint ID (EID) of the bundle"
    )]
    report_to: Option<Eid>,

    #[arg(
        short,
        long,
        long_help = "Path to the file to use as payload, or stdin if not supplied"
    )]
    payload: Option<PathBuf>,

    #[arg(
        short,
        long,
        long_help = "Path to the location to write the bundle to, or stdout if not supplied"
    )]
    output: Option<PathBuf>,

    #[arg(
        short,
        long,
        long_help = "The lifetime of the bundle, or 24 hours if not supplied"
    )]
    lifetime: Option<humantime::Duration>,
}

pub fn exec(args: Command) -> ExitCode {
    let input: &mut dyn BufRead = if let Some(input) = args.payload {
        &mut std::io::BufReader::new({
            match std::fs::File::open(input) {
                Err(e) => {
                    eprintln!("Failed to open input file: {e}");
                    return ExitCode::FAILURE;
                }
                Ok(f) => f,
            }
        })
    } else {
        &mut std::io::BufReader::new(std::io::stdin())
    };

    let mut payload = Vec::new();
    if let Err(e) = input.read_to_end(&mut payload) {
        eprintln!("Failed to read from input: {e}");
        return ExitCode::FAILURE;
    }

    let output: &mut dyn Write = if let Some(output) = args.output {
        &mut BufWriter::new({
            match std::fs::File::create(output) {
                Err(e) => {
                    eprintln!("Failed to open output file: {e}");
                    return ExitCode::FAILURE;
                }
                Ok(f) => f,
            }
        })
    } else {
        &mut BufWriter::new(std::io::stdout())
    };

    let mut builder = hardy_bpv7::builder::Builder::new(args.source, args.destination);

    if let Some(report_to) = args.report_to {
        builder = builder.with_report_to(report_to);
    }

    if let Some(lifetime) = args.lifetime {
        if lifetime.as_millis() > u64::MAX as u128 {
            eprintln!("Lifetime too long!");
            return ExitCode::FAILURE;
        }
        builder = builder.with_lifetime(lifetime.into());
    }

    if let Err(e) = output.write_all(
        &builder
            .add_extension_block(hardy_bpv7::block::Type::Payload)
            .with_flags(hardy_bpv7::block::Flags {
                delete_bundle_on_failure: true,
                ..Default::default()
            })
            .build(&payload)
            .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
            .1,
    ) {
        eprintln!("Failed to write to output: {e}");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
