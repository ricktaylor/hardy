use super::*;
use hardy_async::sync::RwLock;

/// Tracks whether filters modified the bundle or its metadata.
#[derive(Default)]
pub struct Mutation {
    pub bundle: bool,
    pub metadata: bool,
}

/// Result of executing the filter chain on a bundle.
///
/// `Continue` carries the bundle, data, and whether a WriteFilter produced new data.
#[allow(clippy::large_enum_variant)]
pub enum ExecResult {
    Continue(Mutation, bundle::Bundle, Bytes),
    Drop(
        bundle::Bundle,
        Option<hardy_bpv7::status_report::ReasonCode>,
    ),
}

#[derive(Default)]
struct RegistryInner {
    ingress: filter::FilterChain,
    deliver: filter::FilterChain,
    originate: filter::FilterChain,
    egress: filter::FilterChain,
}

impl RegistryInner {
    fn chain(&self, hook: &Hook) -> &filter::FilterChain {
        match hook {
            Hook::Ingress => &self.ingress,
            Hook::Deliver => &self.deliver,
            Hook::Originate => &self.originate,
            Hook::Egress => &self.egress,
        }
    }

    fn chain_mut(&mut self, hook: &Hook) -> &mut filter::FilterChain {
        match hook {
            Hook::Ingress => &mut self.ingress,
            Hook::Deliver => &mut self.deliver,
            Hook::Originate => &mut self.originate,
            Hook::Egress => &mut self.egress,
        }
    }
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
        self.inner
            .write()
            .chain_mut(&hook)
            .add_filter(name, filter, after)?;

        metrics::gauge!("bpa.filter.registered", "hook" => hook.label()).increment(1.0);
        Ok(())
    }

    pub fn unregister(&self, hook: Hook, name: &str) -> Result<Option<Filter>, Error> {
        let result = self.inner.write().chain_mut(&hook).remove_filter(name)?;

        if result.is_some() {
            metrics::gauge!("bpa.filter.registered", "hook" => hook.label()).decrement(1.0);
        }

        Ok(result)
    }

    /// Briefly holds the read lock to prepare (clone Arc refs), then releases
    /// before execution — avoids holding a sync lock across await points.
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
        let prepared = self.inner.read().chain(&hook).prepare();
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
