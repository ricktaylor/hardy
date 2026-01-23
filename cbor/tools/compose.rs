/*!
Compose command - convert various formats to CBOR
*/

use crate::cdn::{self, CdnValue};
use crate::io::{Input, Output};
use clap::Parser;

/// Input format for compose command
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum InputFormat {
    /// CBOR Diagnostic Notation (lossless)
    Cdn,
    /// JSON format (lossy - no tags, limited types)
    Json,
}

/// Convert various text formats to CBOR binary
#[derive(Parser, Debug)]
#[command(
    about = "Convert text formats to CBOR binary",
    long_about = "Parse text in various formats (CDN, JSON) and convert to CBOR binary.\n\n\
                  CDN is lossless and preserves all CBOR semantics.\n\
                  JSON is lossy but convenient for simple data structures."
)]
pub struct Command {
    /// Input format
    #[arg(
        long,
        default_value = "cdn",
        value_name = "FORMAT",
        help = "Input format: cdn (lossless), json (lossy)"
    )]
    format: InputFormat,

    /// Output file (default: stdout)
    #[arg(short = 'o', long)]
    output: Option<Output>,

    /// Input file (use '-' for stdin)
    input: Input,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        // Read input text
        let input_text = self.input.read_to_string()?;

        // Parse to CDN AST based on format
        let ast = match self.format {
            InputFormat::Cdn => {
                // Parse CDN to AST
                cdn::parse(&input_text).map_err(|errors| {
                    // Format parse errors nicely
                    let error_msg = errors
                        .iter()
                        .map(|e| format!("Parse error at {:?}: {}", e.span(), e))
                        .collect::<Vec<_>>()
                        .join("\n");
                    anyhow::anyhow!("Failed to parse CDN:\n{}", error_msg)
                })?
            }
            InputFormat::Json => {
                // Parse JSON and convert to CDN AST
                let json_value: serde_json::Value = serde_json::from_str(&input_text)?;
                json_to_cdn(json_value)?
            }
        };

        // Convert AST to CBOR bytes
        let cbor_bytes = hardy_cbor::encode::emit(&ast).0;

        // Write CBOR to output
        let output = self.output.unwrap_or(Output::Stdout);
        output.write_all(&cbor_bytes)?;

        Ok(())
    }
}

/// Convert a JSON value to CDN AST
///
/// Note: This is a lossy conversion since JSON doesn't support all CBOR features
fn json_to_cdn(value: serde_json::Value) -> anyhow::Result<CdnValue> {
    use serde_json::Value as J;

    Ok(match value {
        J::Null => CdnValue::Null,
        J::Bool(b) => CdnValue::Bool(b),

        J::Number(n) => {
            if let Some(i) = n.as_i64() {
                if i >= 0 {
                    CdnValue::Unsigned(i as u64)
                } else {
                    CdnValue::Negative(i)
                }
            } else if let Some(u) = n.as_u64() {
                CdnValue::Unsigned(u)
            } else if let Some(f) = n.as_f64() {
                CdnValue::Float(f)
            } else {
                anyhow::bail!("Invalid JSON number: {}", n)
            }
        }

        J::String(s) => CdnValue::TextString(s),

        J::Array(arr) => {
            let items: Result<Vec<_>, _> = arr.into_iter().map(json_to_cdn).collect();
            CdnValue::Array(items?)
        }

        J::Object(obj) => {
            let mut pairs = Vec::new();
            for (key, val) in obj {
                // JSON object keys are always strings
                let cdn_key = CdnValue::TextString(key);
                let cdn_val = json_to_cdn(val)?;
                pairs.push((cdn_key, cdn_val));
            }
            CdnValue::Map(pairs)
        }
    })
}
