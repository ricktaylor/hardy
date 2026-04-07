use super::*;
use hardy_async::sync::RwLock;

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
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(RegistryInner::default()),
        }
    }

    pub fn clear(&self) {
        let mut inner = self.inner.write();
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
        let mut inner = self.inner.write();

        match hook {
            Hook::Ingress => inner.ingress.add_filter(name, filter, after)?,
            Hook::Deliver => inner.deliver.add_filter(name, filter, after)?,
            Hook::Originate => inner.originate.add_filter(name, filter, after)?,
            Hook::Egress => inner.egress.add_filter(name, filter, after)?,
        }

        metrics::gauge!("bpa.filter.registered", "hook" => hook.label()).increment(1.0);
        Ok(())
    }

    // Removes a filter by name from the specified hook.
    // Returns Ok(Some(filter)) if found and removed, Ok(None) if not found,
    // or Err(HasDependants) if other filters depend on it.
    pub fn unregister(&self, hook: Hook, name: &str) -> Result<Option<Filter>, Error> {
        let hook_label = hook.label();
        let mut inner = self.inner.write();

        let result = match hook {
            Hook::Ingress => inner.ingress.remove_filter(name)?,
            Hook::Deliver => inner.deliver.remove_filter(name)?,
            Hook::Originate => inner.originate.remove_filter(name)?,
            Hook::Egress => inner.egress.remove_filter(name)?,
        };

        if result.is_some() {
            metrics::gauge!("bpa.filter.registered", "hook" => hook_label).decrement(1.0);
        }

        Ok(result)
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
    ) -> Result<ExecResult, crate::Error>
    where
        F: Fn(&hardy_bpv7::bundle::Bundle, &[u8]) -> Box<dyn hardy_bpv7::bpsec::key::KeySource>
            + Clone
            + Send,
    {
        let hook_label = hook.label();

        let prepared = {
            let inner = self.inner.read();
            match hook {
                Hook::Ingress => inner.ingress.prepare(),
                Hook::Deliver => inner.deliver.prepare(),
                Hook::Originate => inner.originate.prepare(),
                Hook::Egress => inner.egress.prepare(),
            }
        };

        let result = prepared.exec(pool, bundle, data, key_provider).await;

        match &result {
            Ok(ExecResult::Continue(mutation, _, _)) => {
                if mutation.bundle || mutation.metadata {
                    metrics::counter!("bpa.filter.modified", "hook" => hook_label).increment(1);
                }
            }
            Ok(ExecResult::Drop(_, _)) => {
                metrics::counter!("bpa.filter.filtered", "hook" => hook_label).increment(1);
            }
            Err(_) => {
                metrics::counter!("bpa.filter.error", "hook" => hook_label).increment(1);
            }
        }

        result
    }
}
