/*!
Inspect command - display CBOR data in various formats
*/

use super::cdn;
use super::io::{Input, Output};
use base64::prelude::*;
use clap::Parser;
use hardy_cbor::decode::{self, FromCbor, Value};

/// Inspect and display CBOR data
#[derive(Parser, Debug)]
#[command(about = "Inspect and display CBOR information", long_about = None)]
pub struct Command {
    /// Output format
    #[arg(
        long,
        default_value = "diag",
        value_name = "FORMAT",
        help = "Output format: diag/diagnostic (CDN, human-readable), json (lossy), hex"
    )]
    format: OutputFormat,

    /// Automatically decode embedded CBOR byte strings
    #[arg(
        short = 'e',
        long = "decode-embedded",
        help = "Opportunistically decode byte strings as CBOR/sequences (tag 24 and untagged)"
    )]
    decode_embedded: bool,

    /// Output file (default: stdout)
    #[arg(short = 'o', long)]
    output: Option<Output>,

    /// Input CBOR file (use '-' for stdin)
    input: Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    /// CBOR Diagnostic Notation (human-readable, lossless)
    #[value(alias = "diagnostic")]
    Diag,
    /// JSON format (lossy - loses CBOR tags, types, etc.)
    Json,
    /// Hexadecimal dump
    Hex,
}

impl Command {
    pub fn exec(self) -> anyhow::Result<()> {
        let cbor_bytes = self.input.read_all()?;

        let output_text = match self.format {
            OutputFormat::Diag => cdn::format_cbor(&cbor_bytes, self.decode_embedded)?,
            OutputFormat::Json => format_as_json(&cbor_bytes)?,
            OutputFormat::Hex => hex::encode(&cbor_bytes),
        };

        let output = self.output.unwrap_or(Output::Stdout);
        output.write_str(&output_text)?;

        // Add newline for better terminal output
        if matches!(output, Output::Stdout) {
            println!();
        }

        Ok(())
    }
}

/// Format CBOR as JSON (lossy conversion)
fn format_as_json(cbor_bytes: &[u8]) -> anyhow::Result<String> {
    let (json_value, _len) = decode::parse_value(cbor_bytes, |val, _shortest, _tags| {
        value_to_json(val, cbor_bytes)
    })?;

    Ok(json_value)
}

/// Convert a CBOR value to JSON string (lossy)
fn value_to_json(value: Value, data: &[u8]) -> Result<String, decode::Error> {
    match value {
        Value::UnsignedInteger(n) => Ok(n.to_string()),

        Value::NegativeInteger(n) => {
            let actual = -(n as i128 + 1);
            Ok(actual.to_string())
        }

        Value::Bytes(range) => {
            // Convert bytes to base64 string in JSON
            Ok(format!(
                "\"{}\"",
                BASE64_URL_SAFE_NO_PAD.encode(&data[range.clone()])
            ))
        }

        Value::ByteStream(ranges) => {
            let mut all_bytes = Vec::new();
            for range in ranges {
                all_bytes.extend_from_slice(&data[range]);
            }
            Ok(format!("\"{}\"", BASE64_URL_SAFE_NO_PAD.encode(&all_bytes)))
        }

        Value::Text(s) => Ok(format!("\"{}\"", escape_json_string(s))),

        Value::TextStream(chunks) => {
            let text: String = chunks.iter().copied().collect();
            Ok(format!("\"{}\"", escape_json_string(&text)))
        }

        Value::Array(array) => {
            let mut items = Vec::new();
            while let Ok(JsonString(item_json)) = array.parse::<JsonString>() {
                items.push(item_json);
            }
            Ok(format!("[{}]", items.join(", ")))
        }

        Value::Map(map) => {
            let mut pairs = Vec::new();
            while let Ok(JsonString(key_json)) = map.parse::<JsonString>() {
                let JsonString(val_json) = map.parse::<JsonString>()?;
                // In JSON, all keys must be strings
                pairs.push(format!("{}: {}", key_json, val_json));
            }
            Ok(format!("{{{}}}", pairs.join(", ")))
        }

        Value::False => Ok("false".to_string()),
        Value::True => Ok("true".to_string()),
        Value::Null => Ok("null".to_string()),
        Value::Undefined => Ok("null".to_string()), // JSON doesn't have undefined

        Value::Float(f) => {
            if f.is_nan() || f.is_infinite() {
                Ok("null".to_string()) // JSON doesn't support NaN/Infinity
            } else {
                Ok(f.to_string())
            }
        }

        Value::Simple(_) => Ok("null".to_string()), // No JSON equivalent
    }
}

/// Escape special characters for JSON strings
fn escape_json_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\x08' => result.push_str("\\b"),
            '\x0C' => result.push_str("\\f"),
            c if c.is_control() => {
                result.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => result.push(c),
        }
    }
    result
}

/// Wrapper type for parsing CBOR values as JSON strings
struct JsonString(String);

impl FromCbor for JsonString {
    type Error = decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        decode::parse_value(data, |val, shortest, _tags| {
            value_to_json(val, data).map(|s| (JsonString(s), shortest))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}
