use super::*;
use std::sync::RwLock;

#[derive(Default)]
pub struct Mutation {
    pub metadata: bool,
    pub bundle: bool,
}

/// Result of executing the filter chain on a bundle.
#[allow(clippy::large_enum_variant)]
pub enum ExecResult {
    /// Bundle passed all filters; continue processing with (possibly modified) bundle and data.
    Continue(Mutation, bundle::Bundle, Bytes),
    /// Bundle was rejected by a filter; the bundle contains enough information for Dispatcher::drop_bundle to work.
    Drop(
        bundle::Bundle,
        Option<hardy_bpv7::status_report::ReasonCode>,
    ),
}

#[derive(Default)]
struct RegistryInner {
    ingress: filter::FilterNode,
    deliver: filter::FilterNode,
    originate: filter::FilterNode,
    egress: filter::FilterNode,
}

pub struct Registry {
    inner: RwLock<RegistryInner>,
}

impl Registry {
    pub fn new(_config: &config::Config) -> Self {
        Self {
            inner: RwLock::new(RegistryInner::default()),
        }
    }

    pub fn clear(&self) {
        let mut inner = self
            .inner
            .write()
            .trace_expect("Failed to write lock mutex during shutdown");
        inner.ingress.clear();
        inner.deliver.clear();
        inner.originate.clear();
        inner.egress.clear();
    }

    pub fn register(
        &self,
        hook: Hook,
        name: &str,
        after: &[&str],
        filter: Filter,
    ) -> Result<(), Error> {
        let mut inner = self
            .inner
            .write()
            .trace_expect("Failed to write lock mutex");

        match hook {
            Hook::Ingress => inner.ingress.add_filter(name, filter, after),
            Hook::Deliver => inner.deliver.add_filter(name, filter, after),
            Hook::Originate => inner.originate.add_filter(name, filter, after),
            Hook::Egress => inner.egress.add_filter(name, filter, after),
        }
    }

    // Removes a filter by name from the specified hook.
    // Returns Ok(Some(filter)) if found and removed, Ok(None) if not found,
    // or Err(HasDependants) if other filters depend on it.
    pub fn unregister(&self, hook: Hook, name: &str) -> Result<Option<Filter>, Error> {
        let mut inner = self
            .inner
            .write()
            .trace_expect("Failed to write lock mutex");

        match hook {
            Hook::Ingress => inner.ingress.remove_filter(name),
            Hook::Deliver => inner.deliver.remove_filter(name),
            Hook::Originate => inner.originate.remove_filter(name),
            Hook::Egress => inner.egress.remove_filter(name),
        }
    }

    // Executes the filter chain for the specified hook on the given bundle.
    // Briefly holds the read lock to prepare (clone Arc refs), then releases
    // before execution. This avoids holding a sync lock across await points
    // (which isn't Send-safe) and prevents writer starvation.
    // Uses the provided BoundedTaskPool for parallel ReadFilter execution.
    pub async fn exec<F>(
        &self,
        hook: Hook,
        bundle: bundle::Bundle,
        data: Bytes,
        key_provider: F,
        pool: &hardy_async::BoundedTaskPool,
    ) -> Result<ExecResult, bpa::Error>
    where
        F: Fn(&hardy_bpv7::bundle::Bundle, &[u8]) -> Box<dyn hardy_bpv7::bpsec::key::KeySource>
            + Clone
            + Send,
    {
        let prepared = {
            let inner = self.inner.read().trace_expect("Failed to read lock mutex");
            match hook {
                Hook::Ingress => inner.ingress.prepare(),
                Hook::Deliver => inner.deliver.prepare(),
                Hook::Originate => inner.originate.prepare(),
                Hook::Egress => inner.egress.prepare(),
            }
        };
        prepared.exec(pool, bundle, data, key_provider).await
    }
}
