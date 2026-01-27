use clap::{Parser, Subcommand, ValueEnum};

mod add_block;
mod create;
mod encrypt;
mod extract;
mod flags;
mod inspect;
mod io;
mod keys;
mod remove_block;
mod remove_encryption;
mod remove_integrity;
mod rewrite;
mod sign;
mod update_block;
mod validate;
mod verify;

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
    /// Create a new bundle with payload
    Create(create::Command),

    /// Inspect and display bundle information
    Inspect(inspect::Command),

    /// Rewrite a bundle, removing unsupported blocks and canonicalizing
    Rewrite(rewrite::Command),

    /// Check one or more bundles for validity
    Validate(validate::Command),

    /// Encrypt a block using BPSec Block Confidentiality Block (BCB)
    Encrypt(encrypt::Command),

    /// Extract the data from a block in a bundle
    Extract(extract::Command),

    /// Sign a block using BPSec Block Integrity Block (BIB)
    Sign(sign::Command),

    /// Verify the integrity signature of a block
    Verify(verify::Command),

    /// Remove a block from BIB protection
    RemoveIntegrity(remove_integrity::Command),

    /// Decrypt a block and remove it from BCB protection
    RemoveEncryption(remove_encryption::Command),

    /// Add an extension block to a bundle
    AddBlock(add_block::Command),

    /// Remove an extension block from a bundle
    RemoveBlock(remove_block::Command),

    /// Update an existing block in a bundle
    UpdateBlock(update_block::Command),
}

fn main() -> anyhow::Result<()> {
    // Match on the parsed subcommand and call the appropriate handler function.
    // This is the core of the dispatch logic.
    match Cli::parse().command {
        Commands::Create(args) => args.exec(),
        Commands::Inspect(args) => args.exec(),
        Commands::Rewrite(args) => args.exec(),
        Commands::Validate(args) => args.exec(),
        Commands::Encrypt(args) => args.exec(),
        Commands::Extract(args) => args.exec(),
        Commands::Sign(args) => args.exec(),
        Commands::Verify(args) => args.exec(),
        Commands::RemoveIntegrity(args) => args.exec(),
        Commands::RemoveEncryption(args) => args.exec(),
        Commands::AddBlock(args) => args.exec(),
        Commands::RemoveBlock(args) => args.exec(),
        Commands::UpdateBlock(args) => args.exec(),
    }
}
