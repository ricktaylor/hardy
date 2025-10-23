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

pub fn exec(args: Command) -> anyhow::Result<()> {
    let input: &mut dyn BufRead = if let Some(input) = args.payload {
        &mut std::io::BufReader::new(
            std::fs::File::open(input)
                .map_err(|e| anyhow::anyhow!("Failed to open input file: {e}"))?,
        )
    } else {
        &mut std::io::BufReader::new(std::io::stdin())
    };

    let mut payload = Vec::new();
    input
        .read_to_end(&mut payload)
        .map_err(|e| anyhow::anyhow!("Failed to read from input: {e}"))?;

    let output: &mut dyn Write = if let Some(output) = args.output {
        &mut BufWriter::new(
            std::fs::File::create(output)
                .map_err(|e| anyhow::anyhow!("Failed to open output file: {e}"))?,
        )
    } else {
        &mut BufWriter::new(std::io::stdout())
    };

    let mut builder = hardy_bpv7::builder::Builder::new(args.source, args.destination);

    if let Some(report_to) = args.report_to {
        builder = builder.with_report_to(report_to);
    }

    if let Some(lifetime) = args.lifetime {
        (lifetime.as_millis() > u64::MAX as u128)
            .then_some(())
            .ok_or(anyhow::anyhow!("Lifetime too long: {lifetime}!"))?;

        builder = builder.with_lifetime(lifetime.into());
    }

    output
        .write_all(
            &builder
                .add_extension_block(hardy_bpv7::block::Type::Payload)
                .with_flags(hardy_bpv7::block::Flags {
                    delete_bundle_on_failure: true,
                    ..Default::default()
                })
                .build(&payload)
                .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
                .1,
        )
        .map_err(|e| anyhow::anyhow!("Failed to write to output: {e}"))
}
