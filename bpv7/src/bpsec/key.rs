use super::*;

pub trait KeyStore {
    /// Get an iterator for keys suitable for decryption, verification, or unwrapping
    fn decrypt_keys<'a>(
        &'a self,
        source: &eid::Eid,
        operations: &[Operation],
    ) -> impl Iterator<Item = &'a Key>;
}

#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct KeySet {
    pub keys: Vec<Key>,
}

impl KeySet {
    pub const EMPTY: Self = Self { keys: Vec::new() };

    pub fn new(keys: Vec<Key>) -> Self {
        Self { keys }
    }
}

impl KeyStore for KeySet {
    fn decrypt_keys<'a>(
        &'a self,
        _source: &eid::Eid,
        operations: &[Operation],
    ) -> impl Iterator<Item = &'a Key> {
        self.keys.iter().filter(move |k| {
            let Some(key_operations) = &k.operations else {
                return true;
            };
            for op in operations {
                if key_operations.contains(op) {
                    return true;
                }
            }
            false
        })
    }
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct Key {
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub key_type: Type,

    #[cfg_attr(feature = "serde", serde(flatten))]
    pub key_algorithm: Option<KeyAlgorithm>,

    #[cfg_attr(feature = "serde", serde(flatten))]
    pub enc_algorithm: Option<EncAlgorithm>,

    #[cfg_attr(feature = "serde", serde(rename = "key_ops"))]
    pub operations: Option<HashSet<Operation>>,

    /* The following members are standard, but unused in the implementation
     * but here for use by crate users */
    #[cfg_attr(feature = "serde", serde(rename = "kid"))]
    pub id: Option<String>,

    #[cfg_attr(
        feature = "serde",
        serde(rename = "use", skip_serializing_if = "Option::is_none")
    )]
    pub key_use: Option<Use>,
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "kty"))]
pub enum Type {
    #[cfg_attr(feature = "serde", serde(rename = "EC"))]
    EllipticCurve,
    RSA,
    #[cfg_attr(feature = "serde", serde(rename = "oct"))]
    OctetSequence {
        #[cfg_attr(feature = "serde", serde(rename = "k"))]
        #[cfg_attr(
            feature = "serde",
            serde(serialize_with = "serialize_key", deserialize_with = "deserialize_key")
        )]
        key: Box<[u8]>,
    },
    #[default]
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

#[cfg(feature = "serde")]
use base64::prelude::*;

#[cfg(feature = "serde")]
fn serialize_key<S>(k: &[u8], s: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    s.serialize_str(BASE64_URL_SAFE_NO_PAD.encode(k).as_str())
}

#[cfg(feature = "serde")]
fn deserialize_key<'de, D>(deserializer: D) -> core::result::Result<Box<[u8]>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // First, deserialize the YAML value as a simple String.
    let s: String = serde::Deserialize::deserialize(deserializer)?;

    BASE64_URL_SAFE_NO_PAD
        .decode(s.as_bytes())
        .map_err(serde::de::Error::custom)
        .map(Into::into)
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum Use {
    #[cfg_attr(feature = "serde", serde(rename = "sig"))]
    Signature,
    #[cfg_attr(feature = "serde", serde(rename = "enc"))]
    Encryption,
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum Operation {
    #[cfg_attr(feature = "serde", serde(rename = "sign"))]
    Sign,
    #[cfg_attr(feature = "serde", serde(rename = "verify"))]
    Verify,
    #[cfg_attr(feature = "serde", serde(rename = "encrypt"))]
    Encrypt,
    #[cfg_attr(feature = "serde", serde(rename = "decrypt"))]
    Decrypt,
    #[cfg_attr(feature = "serde", serde(rename = "wrapKey"))]
    WrapKey,
    #[cfg_attr(feature = "serde", serde(rename = "unwrapKey"))]
    UnwrapKey,
    #[cfg_attr(feature = "serde", serde(rename = "deriveKey"))]
    DeriveKey,
    #[cfg_attr(feature = "serde", serde(rename = "deriveBits"))]
    DeriveBits,
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "alg"))]
pub enum KeyAlgorithm {
    #[cfg_attr(feature = "serde", serde(rename = "dir"))]
    Direct,
    A128KW,
    A192KW,
    A256KW,
    HS256,
    HS384,
    HS512,
    #[cfg_attr(feature = "serde", serde(rename = "HS256+A128KW"))]
    HS256_A128KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS256+A192KW"))]
    HS256_A192KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS256+A256KW"))]
    HS256_A256KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS384+A128KW"))]
    HS384_A128KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS384+A192KW"))]
    HS384_A192KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS384+A256KW"))]
    HS384_A256KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS512+A128KW"))]
    HS512_A128KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS512+A192KW"))]
    HS512_A192KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS512+A256KW"))]
    HS512_A256KW,
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "enc"))]
pub enum EncAlgorithm {
    A128GCM,
    A256GCM,
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}
