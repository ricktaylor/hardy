use super::*;
use thiserror::Error;

pub(crate) mod registry;

mod filter;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Filter with name '{0}' already exists")]
    AlreadyExists(String),

    #[error("Filter dependency '{0}' not found")]
    DependencyNotFound(String),

    #[error("Filter '{0}' has dependants: {1:?}")]
    HasDependants(String, Vec<String>),
}

// Result types

#[derive(Debug, Default)]
pub enum FilterResult {
    #[default]
    Continue,
    Drop(Option<hardy_bpv7::status_report::ReasonCode>),
}

#[derive(Debug)]
pub enum RewriteResult {
    /// Continue processing, optionally with modified metadata and/or bundle data
    /// - (None, None): no change
    /// - (Some(meta), None): metadata changed, bundle bytes unchanged
    /// - (None, Some(data)): bundle bytes changed (rare)
    /// - (Some(meta), Some(data)): both changed
    Continue(Option<metadata::WritableMetadata>, Option<Box<[u8]>>),
    Drop(Option<hardy_bpv7::status_report::ReasonCode>),
}

// Filter traits

/// Read-only filter: can run in parallel with other ReadFilters
#[async_trait]
pub trait ReadFilter: Send + Sync {
    async fn filter(
        &self,
        bundle: &bundle::Bundle,
        data: &[u8],
    ) -> Result<FilterResult, crate::Error>;
}

/// Read-write filter: runs sequentially, may modify metadata or bundle data
#[async_trait]
pub trait WriteFilter: Send + Sync {
    async fn filter(
        &self,
        bundle: &bundle::Bundle,
        data: &[u8],
    ) -> Result<RewriteResult, crate::Error>;
}

// Registration types

/// Filter wrapper enum for registration
pub enum Filter {
    Read(Arc<dyn ReadFilter>),
    Write(Arc<dyn WriteFilter>),
}

/// Hook points in bundle processing
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
#[derive(Debug)]
pub enum Hook {
    Ingress,
    Deliver,
    Originate,
    Egress,
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for Hook {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "ingress" => Ok(Hook::Ingress),
            "deliver" => Ok(Hook::Deliver),
            "originate" => Ok(Hook::Originate),
            "egress" => Ok(Hook::Egress),
            _ => Err(serde::de::Error::unknown_variant(
                &s,
                &["ingress", "deliver", "originate", "egress"],
            )),
        }
    }
}
