use super::*;

#[cfg(feature = "serde")]
use serde_with::serde_as;

pub trait KeyStore {
    /// Get an iterator for keys suitable for decryption, verification, or unwrapping
    fn decrypt_keys<'a>(
        &'a self,
        source: &eid::Eid,
        operations: &[Operation],
    ) -> impl Iterator<Item = &'a Key>;
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", serde_as)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
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
    #[cfg_attr(feature = "serde", serde_as(as = "serde_with::NoneAsEmptyString"))]
    pub id: Option<String>,

    #[cfg_attr(feature = "serde", serde(rename = "use"))]
    pub key_use: Option<Use>,
}

#[derive(Default, Debug, Clone)]
#[cfg_attr(feature = "serde", serde_as)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
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
            serde_as(
                as = "serde_with::base64::Base64<serde_with::base64::UrlSafe, serde_with::formats::Unpadded>"
            )
        )]
        key: Box<[u8]>,
    },
    #[default]
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
pub enum Use {
    #[cfg_attr(feature = "serde", serde(rename = "sig"))]
    Signature,
    #[cfg_attr(feature = "serde", serde(rename = "enc"))]
    Encryption,
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
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
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
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
    #[cfg_attr(feature = "serde", serde(rename = "HS384+A192KW"))]
    HS384_A192KW,
    #[cfg_attr(feature = "serde", serde(rename = "HS512+A256KW"))]
    HS512_A256KW,
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}

#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(tag = "enc"))]
pub enum EncAlgorithm {
    A128GCM,
    A256GCM,
    #[cfg_attr(feature = "serde", serde(other))]
    Unknown,
}
