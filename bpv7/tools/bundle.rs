use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod create;
mod validate;

/// A CLI tool for creating and managing BPv7 bundles.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// The subcommand to execute.
    #[command(subcommand)]
    command: Commands,
}

/// Defines the available subcommands for the 'bundle' tool.
#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a new BPv7 bundle from a payload file.
    Create(create::Command),

    /// Check one or more bundles for validity.
    Validate(validate::Command),
}

fn main() -> anyhow::Result<()> {
    // Match on the parsed subcommand and call the appropriate handler function.
    // This is the core of the dispatch logic.
    match Cli::parse().command {
        Commands::Create(args) => create::exec(args),
        Commands::Validate(args) => validate::exec(args),
    }
}
