use clap::{Parser, Subcommand, ValueEnum};

mod ping;

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
    /// Send a 'ping' bundle to a remote node
    Ping(ping::Command),
}

fn main() -> anyhow::Result<()> {
    // Match on the parsed subcommand and call the appropriate handler function.
    // This is the core of the dispatch logic.
    match Cli::parse().command {
        Commands::Ping(args) => args.exec(),
    }
}
