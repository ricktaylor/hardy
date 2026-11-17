use hardy_bpa::{Error, cla::Registration};
#[cfg(feature = "file-cla")]
use hardy_file_cla::Config as FileClaConfig;
#[cfg(feature = "tcpclv4")]
use hardy_tcpclv4::config::Config as Tcpclv4Config;
use serde::{Deserialize, Serialize};
use thiserror::Error as ThisError;

// `deny_unknown_fields` does not compose with `flatten`, but strictness
// holds anyway: every key that is not `name` or `policy` is forwarded to
// the flattened `ClaType`, whose payloads are strict for known types.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClaConfig {
    pub name: String,
    #[serde(flatten)]
    pub cla_type: ClaType,
    #[serde(default)]
    pub policy: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
pub enum ClaType {
    #[cfg(feature = "tcpclv4")]
    #[serde(rename = "tcpclv4")]
    TcpClv4(Tcpclv4Config),

    #[cfg(feature = "file-cla")]
    #[serde(rename = "file-cla")]
    File(FileClaConfig),

    #[serde(untagged)]
    Other {
        #[serde(rename = "type")]
        cla_type: String,
        #[serde(flatten)]
        config: serde_json::Value,
    },
}

// Unknown CLA types are tolerated (`Other`, ignored with a warning at
// assembly) so a config can name extension CLAs this binary was not built
// with, but a known type with a malformed payload must fail loudly. A
// derived untagged fallback cannot tell the two apart: it swallows the
// payload's parse error and ignores the whole entry, so the dispatch on
// `type` is by hand.
impl<'de> Deserialize<'de> for ClaType {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut entry = serde_json::Map::deserialize(deserializer)?;
        let cla_type = match entry.remove("type") {
            Some(serde_json::Value::String(cla_type)) => cla_type,
            Some(_) => return Err(serde::de::Error::custom("type must be a string")),
            None => return Err(serde::de::Error::missing_field("type")),
        };
        let config = serde_json::Value::Object(entry);
        match cla_type.as_str() {
            #[cfg(feature = "tcpclv4")]
            "tcpclv4" => serde_json::from_value(config)
                .map(Self::TcpClv4)
                .map_err(serde::de::Error::custom),
            #[cfg(feature = "file-cla")]
            "file-cla" => serde_json::from_value(config)
                .map(Self::File)
                .map_err(serde::de::Error::custom),
            _ => Ok(Self::Other { cla_type, config }),
        }
    }
}

impl ClaType {
    // The `type` of an entry this build does not know, which is skipped;
    // `None` for a known type.
    pub fn unknown_type(&self) -> Option<&str> {
        match self {
            Self::Other { cla_type, .. } => Some(cla_type),
            #[allow(unreachable_patterns)] // only `Other` without a CLA feature
            _ => None,
        }
    }
}

// Converting an entry of a CLA type this build does not have: there is
// nothing to build.
#[derive(Debug, ThisError, PartialEq, Eq)]
#[error("unknown CLA type '{0}'")]
pub struct UnknownClaType(pub String);

// Each known type converts through its own crate's `Registration`
// conversion, so `BpaBuilder::cla` builds a configured CLA without this
// crate knowing how. `BpaServer::new` skips `Other` entries before
// registering, with a warning; converting one is `UnknownClaType`.
impl TryFrom<ClaType> for Registration {
    type Error = Error;

    fn try_from(cla_type: ClaType) -> Result<Self, Self::Error> {
        match cla_type {
            #[cfg(feature = "tcpclv4")]
            ClaType::TcpClv4(config) => Ok(Registration::try_from(config)?),
            #[cfg(feature = "file-cla")]
            ClaType::File(config) => Ok(Registration::try_from(config)?),
            ClaType::Other { cla_type, .. } => Err(Box::new(UnknownClaType(cla_type))),
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "tcpclv4")]
    use core::num::NonZeroU64;

    #[cfg(feature = "tcpclv4")]
    use hardy_tcpclv4::{Error as Tcpclv4Error, tls::Error as TlsError};
    use serde_json::json;

    use super::*;

    fn cla_type(entry: serde_json::Value) -> ClaType {
        serde_json::from_value(entry).unwrap()
    }

    #[test]
    fn unknown_type_names_only_unknown_entries() {
        let other = cla_type(json!({ "type": "extension-cla", "key": 1 }));
        assert_eq!(other.unknown_type(), Some("extension-cla"));
        let Err(err) = Registration::try_from(other) else {
            panic!("an unknown type has nothing to build");
        };
        assert_eq!(
            err.downcast_ref::<UnknownClaType>(),
            Some(&UnknownClaType("extension-cla".into()))
        );
    }

    // A dial-only plaintext entry builds without binding a socket, and
    // carries the CLA's own declarations.
    #[cfg(feature = "tcpclv4")]
    #[test]
    fn tcpclv4_entry_converts_with_its_declarations() {
        let entry = cla_type(json!({
            "type": "tcpclv4",
            "listeners": [],
            "transfer-mru": 65536,
        }));
        assert_eq!(entry.unknown_type(), None);
        let registration = Registration::try_from(entry).unwrap();
        assert_eq!(registration.init.max_bundle_size, NonZeroU64::new(65536));
    }

    // The tcpclv4 build error passes through, so `BpaBuilder::build`
    // reports it against the CLA's name.
    #[cfg(feature = "tcpclv4")]
    #[test]
    fn tcpclv4_entry_build_error_is_the_crate_error() {
        let entry = cla_type(json!({ "type": "tcpclv4", "listeners": [], "tls": {} }));
        let Err(err) = Registration::try_from(entry) else {
            panic!("TLS without a trust anchor must fail");
        };
        assert!(
            matches!(
                err.downcast_ref::<Tcpclv4Error>(),
                Some(Tcpclv4Error::Tls(TlsError::NoTrustAnchor))
            ),
            "{err:?}"
        );
    }
}
