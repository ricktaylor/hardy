use clap::{Parser, Subcommand};

mod cmd;

/// A CLI tool for creating and managing BPv7 bundles.
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Create a new bundle with payload
    Create(cmd::create::Command),
    /// Inspect and display bundle information
    Inspect(cmd::inspect::Command),
    /// Rewrite a bundle, removing unsupported blocks and canonicalizing
    Rewrite(cmd::rewrite::Command),
    /// Check one or more bundles for validity
    Validate(cmd::validate::Command),
    /// Encrypt a block using BPSec Block Confidentiality Block (BCB)
    Encrypt(cmd::encrypt::Command),
    /// Extract the data from a block in a bundle
    Extract(cmd::extract::Command),
    /// Sign a block using BPSec Block Integrity Block (BIB)
    Sign(cmd::sign::Command),
    /// Verify the integrity signature of a block
    Verify(cmd::verify::Command),
    /// Remove a block from BIB protection
    RemoveIntegrity(cmd::remove_integrity::Command),
    /// Decrypt a block and remove it from BCB protection
    RemoveEncryption(cmd::remove_encryption::Command),
    /// Add an extension block to a bundle
    AddBlock(cmd::add_block::Command),
    /// Remove an extension block from a bundle
    RemoveBlock(cmd::remove_block::Command),
    /// Update an existing block in a bundle
    UpdateBlock(cmd::update_block::Command),
    /// Update the primary block of a bundle
    UpdatePrimary(cmd::update_primary::Command),
    /// Compare two bundles for semantic equivalence
    Compare(cmd::compare::Command),
}

fn main() -> anyhow::Result<()> {
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
        Commands::UpdatePrimary(args) => args.exec(),
        Commands::Compare(args) => args.exec(),
    }
}
