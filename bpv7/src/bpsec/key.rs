use super::*;
use serde::Deserialize;
use serde_with::{
    NoneAsEmptyString,
    base64::{Base64, UrlSafe},
    formats::Unpadded,
    serde_as,
};

pub trait KeyStore {
    /// Get an iterator for keys suitable for decryption, verification, or unwrapping
    fn decrypt_keys<'a>(
        &'a self,
        source: &eid::Eid,
        operations: &[Operation],
    ) -> impl Iterator<Item = &'a Key>;
}

#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct Key {
    #[serde(flatten)]
    pub key_type: Type,

    #[serde(flatten)]
    pub key_algorithm: Option<KeyAlgorithm>,

    #[serde(flatten)]
    pub enc_algorithm: Option<EncAlgorithm>,

    #[serde(rename = "key_ops")]
    pub operations: Option<HashSet<Operation>>,

    /* The following members are standard, but unused in the implementation
     * but here for use by crate users */
    #[serde(rename = "kid")]
    #[serde_as(as = "NoneAsEmptyString")]
    pub id: Option<String>,

    #[serde(rename = "use")]
    pub key_use: Option<Use>,
}

#[serde_as]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kty")]
pub enum Type {
    #[serde(rename = "EC")]
    EllipticCurve,
    RSA,
    #[serde(rename = "oct")]
    OctetSequence {
        #[serde(rename = "k")]
        #[serde_as(as = "Base64<UrlSafe, Unpadded>")]
        key: Vec<u8>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq)]
pub enum Use {
    #[serde(rename = "sig")]
    Signature,
    #[serde(rename = "enc")]
    Encryption,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Hash)]
pub enum Operation {
    #[serde(rename = "sign")]
    Sign,
    #[serde(rename = "verify")]
    Verify,
    #[serde(rename = "encrypt")]
    Encrypt,
    #[serde(rename = "decrypt")]
    Decrypt,
    #[serde(rename = "wrapKey")]
    WrapKey,
    #[serde(rename = "unwrapKey")]
    UnwrapKey,
    #[serde(rename = "deriveKey")]
    DeriveKey,
    #[serde(rename = "deriveBits")]
    DeriveBits,
    #[serde(other)]
    Unknown,
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "alg")]
pub enum KeyAlgorithm {
    #[serde(rename = "dir")]
    Direct,
    A128KW,
    A192KW,
    A256KW,
    HS256,
    HS384,
    HS512,
    #[serde(rename = "HS256+A128KW")]
    HS256_A128KW,
    #[serde(rename = "HS384+A192KW")]
    HS384_A192KW,
    #[serde(rename = "HS512+A256KW")]
    HS512_A256KW,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "enc")]
pub enum EncAlgorithm {
    A128GCM,
    A256GCM,
    #[serde(other)]
    Unknown,
}
