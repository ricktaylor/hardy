use clap::{Parser, Subcommand};

mod create;
mod dump;
mod extract;
mod io;
mod keys;
mod rewrite;
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

    /// Rewrite a bundle, removing unsupported blocks and canonicalizing as appropriate
    Rewrite(rewrite::Command),

    /// Check one or more bundles for validity.
    Validate(validate::Command),

    /// Dump the json serialization of a bundle
    Dump(dump::Command),

    /// Extract the data of a block in the bundle
    Extract(extract::Command),
}

fn main() -> anyhow::Result<()> {
    // Match on the parsed subcommand and call the appropriate handler function.
    // This is the core of the dispatch logic.
    match Cli::parse().command {
        Commands::Create(args) => args.exec(),
        Commands::Rewrite(args) => args.exec(),
        Commands::Dump(args) => args.exec(),
        Commands::Validate(args) => args.exec(),
        Commands::Extract(args) => args.exec(),
        // Extend,
        // Sign,
        // Verify,
    }
}
