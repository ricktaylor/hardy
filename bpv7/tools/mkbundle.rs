use clap::Parser;
use hardy_bpv7::prelude::*;
use std::{
    io::{BufRead, BufWriter, Write},
    path::PathBuf,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    source: Eid,

    #[arg(short, long)]
    destination: Eid,

    #[arg(short, long = "report-to")]
    report_to: Option<Eid>,

    #[arg(short, long)]
    input: Option<PathBuf>,

    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() {
    let args = Args::parse();

    let input: &mut dyn BufRead = if let Some(input) = args.input {
        &mut std::io::BufReader::new(std::fs::File::open(input).expect("Failed to open input file"))
    } else {
        &mut std::io::BufReader::new(std::io::stdin())
    };

    let mut payload = Vec::new();
    input
        .read_to_end(&mut payload)
        .expect("Failed to read from input");

    let mut b = Builder::new()
        .source(args.source)
        .destination(args.destination);

    if let Some(report_to) = args.report_to {
        b = b.report_to(report_to);
    }

    b = b.add_payload_block(payload);

    let (_, data) = b.build().expect("Failed to build bundle");

    let output: &mut dyn Write = if let Some(output) = args.output {
        &mut BufWriter::new(std::fs::File::create(output).expect("Failed to create output file"))
    } else {
        &mut BufWriter::new(std::io::stdout())
    };

    output.write_all(&data).expect("Failed to write bundle")
}
