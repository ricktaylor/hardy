use hardy_bpv7::bpsec::key::{Key, KeySet};

#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum JwkSetInput {
    Key(Key),
    Set(KeySet),
}

impl JwkSetInput {
    /// Helper to always return a KeySet
    fn into_key_set(self) -> KeySet {
        match self {
            JwkSetInput::Key(jwk) => KeySet::new(vec![jwk]),
            JwkSetInput::Set(keyset) => keyset,
        }
    }
}

/// A reusable argument struct for loading a JWK/JWKS.
/// Any subcommand can include this using `#[command(flatten)]`.
#[derive(clap::Args, Debug)]
pub struct KeySetLoaderArgs {
    /// The optional key or key set.
    /// Can be a file path or a raw JSON string.
    #[arg(short, long, value_name = "KEY_OR_KEY_SET_SOURCE")]
    pub keys: Option<String>,
}

impl TryFrom<KeySetLoaderArgs> for KeySet {
    type Error = anyhow::Error;

    fn try_from(args: KeySetLoaderArgs) -> Result<Self, Self::Error> {
        let Some(source) = args.keys else {
            return Ok(KeySet::EMPTY);
        };

        // Get the JSON content string
        let json_content = if source.trim_ascii_start().starts_with(['{', '[']) {
            // Source is a raw JSON string
            source.to_string()
        } else {
            // Source is a file path
            std::fs::read_to_string(source)
                .map_err(|e| anyhow::anyhow!("Failed to read key file: {e}"))?
        };

        // Parse into the enum
        let input: JwkSetInput = serde_json::from_str(&json_content)
            .map_err(|e| anyhow::anyhow!("Failed to parse key: {e}"))?;

        // Normalize to a KeySet and return
        Ok(input.into_key_set())
    }
}
