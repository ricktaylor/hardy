use hardy_async::sync::RwLock;
use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;

use super::chain::{FilterChain, FilterChainBuilder};
use super::{Error, ExecResult, Filter, Hook};
use crate::bundle::Bundle;
use crate::{Arc, Bytes};

/// Built filter chains for all hooks, ready to execute.
#[derive(Default)]
struct Filters {
    ingress: FilterChain,
    deliver: FilterChain,
    originate: FilterChain,
    egress: FilterChain,
}

impl Filters {
    fn chain(&self, hook: &Hook) -> &FilterChain {
        match hook {
            Hook::Ingress => &self.ingress,
            Hook::Deliver => &self.deliver,
            Hook::Originate => &self.originate,
            Hook::Egress => &self.egress,
        }
    }
}

struct RegistryInner {
    ingress: FilterChainBuilder,
    deliver: FilterChainBuilder,
    originate: FilterChainBuilder,
    egress: FilterChainBuilder,

    /// Current filter chain state, rebuilt on register/unregister.
    filters: Arc<Filters>,
}

impl Default for RegistryInner {
    fn default() -> Self {
        Self {
            ingress: FilterChainBuilder::default(),
            deliver: FilterChainBuilder::default(),
            originate: FilterChainBuilder::default(),
            egress: FilterChainBuilder::default(),
            filters: Arc::new(Filters::default()),
        }
    }
}

impl RegistryInner {
    fn builder_mut(&mut self, hook: &Hook) -> &mut FilterChainBuilder {
        match hook {
            Hook::Ingress => &mut self.ingress,
            Hook::Deliver => &mut self.deliver,
            Hook::Originate => &mut self.originate,
            Hook::Egress => &mut self.egress,
        }
    }

    fn rebuild(&mut self) {
        self.filters = Arc::new(Filters {
            ingress: self.ingress.build(),
            deliver: self.deliver.build(),
            originate: self.originate.build(),
            egress: self.egress.build(),
        });
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
        *self.inner.write() = RegistryInner::default();
    }

    pub fn register(
        &self,
        hook: Hook,
        name: &str,
        after: &[&str],
        filter: Filter,
    ) -> Result<(), Error> {
        let mut inner = self.inner.write();
        inner.builder_mut(&hook).add_filter(name, filter, after)?;
        inner.rebuild();

        metrics::gauge!("bpa.filter.registered", "hook" => hook.label()).increment(1.0);
        Ok(())
    }

    pub fn unregister(&self, hook: Hook, name: &str) -> Result<Option<Filter>, Error> {
        let mut inner = self.inner.write();
        let result = inner.builder_mut(&hook).remove_filter(name)?;

        if result.is_some() {
            inner.rebuild();
            metrics::gauge!("bpa.filter.registered", "hook" => hook.label()).decrement(1.0);
        }

        Ok(result)
    }

    /// Grabs the current filters (single Arc clone), then executes
    /// without holding any lock.
    pub async fn exec<F>(
        &self,
        hook: Hook,
        bundle: Bundle,
        data: Bytes,
        key_provider: F,
        pool: &hardy_async::BoundedTaskPool,
    ) -> Result<ExecResult, crate::Error>
    where
        F: Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource> + Clone + Send,
    {
        let hook_label = hook.label();
        let filters = self.inner.read().filters.clone();
        let result = filters
            .chain(&hook)
            .exec(pool, bundle, data, key_provider)
            .await;

        match &result {
            Ok(ExecResult::Continue(mutation, _, _)) => {
                if mutation.data || mutation.metadata {
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
