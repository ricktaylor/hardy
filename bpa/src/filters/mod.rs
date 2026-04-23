use hardy_async::async_trait;
use hardy_bpv7::status_report::ReasonCode;
use thiserror::Error;

use crate::Arc;
use crate::bundle::{Bundle, WritableMetadata};

pub(crate) mod registry;

mod filter;

/// RFC9171 validity filter - always available, auto-registered by default.
/// Disable auto-registration with `no-rfc9171-autoregister` feature.
pub mod rfc9171;

/// Errors related to filter registration and dependency management.
#[derive(Debug, Error)]
pub enum Error {
    /// A filter with the given name is already registered.
    #[error("Filter with name '{0}' already exists")]
    AlreadyExists(String),

    /// A filter declares a dependency on another filter that has not been registered.
    #[error("Filter dependency '{0}' not found")]
    DependencyNotFound(String),

    /// Cannot remove a filter because other filters depend on it.
    #[error("Filter '{0}' has dependants: {1:?}")]
    HasDependants(String, Vec<String>),
}

/// Outcome of a read-only filter evaluation.
#[derive(Debug, Default)]
pub enum FilterResult {
    /// Allow the bundle to proceed to the next filter or processing stage.
    #[default]
    Continue,
    /// Drop the bundle with a status-report reason code.
    Drop(ReasonCode),
}

/// Outcome of a read-write filter evaluation, which may modify the bundle.
#[derive(Debug)]
pub enum RewriteResult {
    /// Continue processing, optionally with modified metadata and/or bundle data
    /// - (None, None): no change
    /// - (Some(meta), None): metadata changed, bundle bytes unchanged
    /// - (None, Some(data)): bundle bytes changed (rare)
    /// - (Some(meta), Some(data)): both changed
    Continue(Option<WritableMetadata>, Option<Box<[u8]>>),
    /// Drop the bundle with a status-report reason code.
    Drop(ReasonCode),
}

// Filter traits

/// Read-only filter: can run in parallel with other ReadFilters
#[async_trait]
pub trait ReadFilter: Send + Sync {
    async fn filter(&self, bundle: &Bundle, data: &[u8]) -> Result<FilterResult, crate::Error>;
}

/// Read-write filter: runs sequentially, may modify metadata or bundle data
#[async_trait]
pub trait WriteFilter: Send + Sync {
    async fn filter(&self, bundle: &Bundle, data: &[u8]) -> Result<RewriteResult, crate::Error>;
}

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

impl Hook {
    /// Returns the lowercase string label for this hook point (e.g. `"ingress"`).
    pub fn label(&self) -> &'static str {
        match self {
            Hook::Ingress => "ingress",
            Hook::Deliver => "deliver",
            Hook::Originate => "originate",
            Hook::Egress => "egress",
        }
    }
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
