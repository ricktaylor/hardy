/*!
CBOR Tools - A CLI for working with CBOR data

This tool provides utilities for inspecting CBOR data and converting between
CBOR binary format and CBOR Diagnostic Notation (CDN).

# Commands

- `inspect`: Display CBOR data in various formats (CDN, JSON, hex)
- `compose`: Convert text formats (CDN, JSON) to CBOR binary

# Examples

```bash
# Inspect a CBOR file (CDN format - lossless)
cbor inspect bundle.cbor

# Inspect with embedded CBOR decoding (decodes byte strings as CBOR - useful for BPv7!)
cbor inspect -e bundle.cbor

# Inspect as JSON (lossy)
cbor inspect --format json data.cbor

# Inspect as hex dump
cbor inspect --format hex data.cbor

# Convert CDN to CBOR (default)
echo '[1, 2, h"deadbeef"]' | cbor compose -o data.cbor

# Convert JSON to CBOR
echo '{"name": "Alice", "age": 30}' | cbor compose --format json -o data.cbor

# Round-trip test
cbor inspect data.cbor | cbor compose | cbor inspect
```
*/

use clap::{Parser, Subcommand};

mod cdn;
mod compose;
mod inspect;
mod io;

/// A CLI tool for working with CBOR data
#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "A CLI tool for inspecting and manipulating CBOR data",
    long_about = "CBOR Tools provides utilities for working with CBOR (Concise Binary Object Representation) data.\n\n\
                  Features:\n\
                  - Inspect CBOR data in human-readable formats\n\
                  - Convert between CBOR and CBOR Diagnostic Notation (CDN)\n\
                  - Lossless round-trip conversion (CBOR ↔ CDN)\n\
                  - Support for all CBOR features (tags, indefinite-length containers, etc.)"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

/// Available subcommands
#[derive(Subcommand, Debug)]
enum Commands {
    /// Inspect and display CBOR data in various formats
    Inspect(inspect::Command),

    /// Convert CBOR Diagnostic Notation (CDN) to CBOR binary
    Compose(compose::Command),
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Commands::Inspect(args) => args.exec(),
        Commands::Compose(args) => args.exec(),
    }
}
