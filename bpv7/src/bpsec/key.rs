use super::*;

/// Resolves cryptographic keys for BPSec operations by source EID and required operations.
pub trait KeySource {
    /// Get the key suitable for the specified operations from the given source.
    /// Returns None if no key is available for this source/operations.
    /// If a key is returned, it is expected to be valid for the operation;
    /// verification/decryption failure with a provided key indicates corruption.
    fn key<'a>(&'a self, source: &eid::Eid, operations: &[Operation]) -> Option<&'a Key>;
}

/// A collection of cryptographic keys that implements [`KeySource`] by linear search.
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct KeySet {
    /// The keys available for BPSec operations.
    pub keys: Vec<Key>,
}

impl KeySet {
    pub const EMPTY: Self = Self { keys: Vec::new() };

    pub fn new(keys: Vec<Key>) -> Self {
        Self { keys }
    }
}

impl KeySource for KeySet {
    fn key<'a>(&'a self, _source: &eid::Eid, operations: &[Operation]) -> Option<&'a Key> {
        self.keys.iter().find(|k| {
            if let Some(key_operations) = &k.operations {
                operations.iter().any(|op| key_operations.contains(op))
            } else {
                false
            }
        })
    }
}

/// A cryptographic key in JWK-like representation for BPSec operations.
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub struct Key {
    /// The key type (e.g., symmetric octet sequence, elliptic curve, RSA).
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub key_type: Type,

    /// The key management algorithm, if applicable.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub key_algorithm: Option<KeyAlgorithm>,

    /// The content encryption algorithm, if applicable.
    #[cfg_attr(feature = "serde", serde(flatten))]
    pub enc_algorithm: Option<EncAlgorithm>,

    /// Permitted operations for this key (e.g., sign, verify, encrypt, decrypt).
    #[cfg_attr(feature = "serde", serde(rename = "key_ops"))]
    pub operations: Option<HashSet<Operation>>,

    /// Optional key identifier (JWK `kid`). Not used internally; available for crate users.
    #[cfg_attr(feature = "serde", serde(rename = "kid"))]
    pub id: Option<String>,

    /// Intended key usage (JWK `use`). Not used internally; available for crate users.
    #[cfg_attr(
        feature = "serde",
        serde(rename = "use", skip_serializing_if = "Option::is_none")
    )]
    pub key_use: Option<Use>,
}

/// JWK key type (`kty`) as defined in RFC 7517.
#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "kty"))]
pub enum Type {
    /// Elliptic Curve key (`EC`).
    #[cfg_attr(feature = "serde", serde(rename = "EC"))]
    EllipticCurve,
    /// RSA key.
    RSA,
    /// Symmetric octet sequence (`oct`), used by HMAC-SHA2 and AES-GCM contexts.
    #[cfg_attr(feature = "serde", serde(rename = "oct"))]
    OctetSequence {
        /// The raw symmetric key bytes, base64url-encoded for serialization.
        #[cfg_attr(feature = "serde", serde(rename = "k"))]
        #[cfg_attr(
            feature = "serde",
            serde(serialize_with = "serialize_key", deserialize_with = "deserialize_key")
        )]
        key: Box<[u8]>,
    },
    /// Unrecognized key type.
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
fn deserialize_key<'de, D>(deserializer: D) -> Result<Box<[u8]>, D::Error>
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

/// JWK public key use (`use`) as defined in RFC 7517 Section 4.2.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum Use {
    /// Key is intended for digital signatures or MACs.
    #[cfg_attr(feature = "serde", serde(rename = "sig"))]
    Signature,
    /// Key is intended for encryption.
    #[cfg_attr(feature = "serde", serde(rename = "enc"))]
    Encryption,
    /// Unrecognized use value.
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

/// JWK key operation (`key_ops`) as defined in RFC 7517 Section 4.3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum Operation {
    /// Compute digital signature or MAC.
    #[cfg_attr(feature = "serde", serde(rename = "sign"))]
    Sign,
    /// Verify digital signature or MAC.
    #[cfg_attr(feature = "serde", serde(rename = "verify"))]
    Verify,
    /// Encrypt content.
    #[cfg_attr(feature = "serde", serde(rename = "encrypt"))]
    Encrypt,
    /// Decrypt content and validate decryption, if applicable.
    #[cfg_attr(feature = "serde", serde(rename = "decrypt"))]
    Decrypt,
    /// Encrypt key.
    #[cfg_attr(feature = "serde", serde(rename = "wrapKey"))]
    WrapKey,
    /// Decrypt key.
    #[cfg_attr(feature = "serde", serde(rename = "unwrapKey"))]
    UnwrapKey,
    /// Derive key.
    #[cfg_attr(feature = "serde", serde(rename = "deriveKey"))]
    DeriveKey,
    /// Derive bits not to be used as a key.
    #[cfg_attr(feature = "serde", serde(rename = "deriveBits"))]
    DeriveBits,
    /// Unrecognized operation.
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

/// Key management algorithm (`alg`) for BPSec key wrapping and HMAC operations.
#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "alg"))]
pub enum KeyAlgorithm {
    /// Direct use of a shared symmetric key (no key wrapping).
    #[cfg_attr(feature = "serde", serde(rename = "dir"))]
    Direct,
    /// AES-128 Key Wrap.
    A128KW,
    /// AES-192 Key Wrap.
    A192KW,
    /// AES-256 Key Wrap.
    A256KW,
    /// HMAC using SHA-256.
    HS256,
    /// HMAC using SHA-384.
    HS384,
    /// HMAC using SHA-512.
    HS512,
    /// HMAC-SHA256 key derivation with AES-128 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS256+A128KW"))]
    HS256_A128KW,
    /// HMAC-SHA256 key derivation with AES-192 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS256+A192KW"))]
    HS256_A192KW,
    /// HMAC-SHA256 key derivation with AES-256 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS256+A256KW"))]
    HS256_A256KW,
    /// HMAC-SHA384 key derivation with AES-128 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS384+A128KW"))]
    HS384_A128KW,
    /// HMAC-SHA384 key derivation with AES-192 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS384+A192KW"))]
    HS384_A192KW,
    /// HMAC-SHA384 key derivation with AES-256 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS384+A256KW"))]
    HS384_A256KW,
    /// HMAC-SHA512 key derivation with AES-128 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS512+A128KW"))]
    HS512_A128KW,
    /// HMAC-SHA512 key derivation with AES-192 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS512+A192KW"))]
    HS512_A192KW,
    /// HMAC-SHA512 key derivation with AES-256 Key Wrap.
    #[cfg_attr(feature = "serde", serde(rename = "HS512+A256KW"))]
    HS512_A256KW,
    /// Unrecognized algorithm.
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

/// Content encryption algorithm (`enc`) for BPSec confidentiality operations.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
#[cfg_attr(feature = "serde", serde(tag = "enc"))]
pub enum EncAlgorithm {
    /// AES-128 in Galois/Counter Mode.
    A128GCM,
    /// AES-256 in Galois/Counter Mode.
    A256GCM,
    /// Unrecognized encryption algorithm.
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}
