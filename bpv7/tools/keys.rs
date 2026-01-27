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
/// Any subcommand can include this using `#[clap(flatten)]`.
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
        let json_content = if source.trim_ascii_start().starts_with('{') {
            // Source is a raw JSON string
            source.to_string()
        } else {
            // Source is a file path
            std::fs::read_to_string(source)
                .map_err(|e| anyhow::anyhow!("Failed to read key file: {e}"))?
        };

        // Parse into the enum
        let input: JwkSetInput = serde_json::from_str(&json_content)
            .map_err(|_| anyhow::anyhow!("Failed to parse key, expecting JWK or JWKS format"))?;

        // Normalize to a KeySet and return
        Ok(input.into_key_set())
    }
}

#[derive(clap::Args, Debug)]
#[group(multiple = false)]
pub struct KeyInput {
    /// The key to use for the operation.
    /// Can be a file path or a raw JSON string.
    #[arg(long, value_name = "KEY_OR_KEY_SOURCE")]
    key: Option<String>,

    #[clap(flatten)]
    keyset_input: Option<KeySetInput>,
}

#[derive(clap::Args, Debug)]
struct KeySetInput {
    /// The optional key or key set.
    /// Can be a file path or a raw JSON string.
    #[arg(long, value_name = "KEYS_OR_KEY_SET_SOURCE")]
    keys: String,

    /// The Key ID (KID) to use for signing from the loaded KeySet. Requires --keys.
    #[arg(long, value_name = "KEY_ID")]
    kid: String,
}

impl TryFrom<KeyInput> for Key {
    type Error = anyhow::Error;

    fn try_from(args: KeyInput) -> Result<Self, Self::Error> {
        let (_, key) = args.try_into_keyset_and_key()?;
        Ok(key)
    }
}

impl KeyInput {
    /// Convert to both a KeySet (for parsing bundles) and the selected Key (for operations).
    /// When --keys is used, the full keyset is available for parsing encrypted bundles,
    /// and the key specified by --kid is used for the operation.
    /// When --key is used, a keyset containing just that key is returned.
    pub fn try_into_keyset_and_key(self) -> anyhow::Result<(KeySet, Key)> {
        if let Some(raw_key_str) = self.key {
            let key_content = if raw_key_str.trim_ascii_start().starts_with('{') {
                raw_key_str
            } else {
                std::fs::read_to_string(&raw_key_str)
                    .map_err(|e| anyhow::anyhow!("Failed to read key file: {e}"))?
            };
            let key: Key = serde_json::from_str(&key_content)
                .map_err(|e| anyhow::anyhow!("Failed to parse key: {e}"))?;
            let keyset = KeySet::new(vec![key.clone()]);
            Ok((keyset, key))
        } else if let Some(keyset_input) = self.keyset_input {
            let keyset: KeySet = KeySetLoaderArgs {
                keys: Some(keyset_input.keys),
            }
            .try_into()?;
            let key = keyset
                .keys
                .iter()
                .find(|k| k.id.as_ref() == Some(&keyset_input.kid))
                .cloned()
                .ok_or(anyhow::anyhow!(
                    "Key '{}' not found in keyset",
                    keyset_input.kid
                ))?;
            Ok((keyset, key))
        } else {
            Err(anyhow::anyhow!(
                "Either --key must be provided, or both --keys and --kid must be provided."
            ))
        }
    }
}
