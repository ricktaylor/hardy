use arc_swap::ArcSwap;
use hardy_async::sync::Mutex;

use super::chain::{FilterChain, FilterChainBuilder};
use super::{Error, ExecResult, Filter, Hook};
use crate::bundle::Bundle;
use crate::key::KeyStore;
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

#[derive(Default)]
struct Builders {
    ingress: FilterChainBuilder,
    deliver: FilterChainBuilder,
    originate: FilterChainBuilder,
    egress: FilterChainBuilder,
}

impl Builders {
    fn get_mut(&mut self, hook: &Hook) -> &mut FilterChainBuilder {
        match hook {
            Hook::Ingress => &mut self.ingress,
            Hook::Deliver => &mut self.deliver,
            Hook::Originate => &mut self.originate,
            Hook::Egress => &mut self.egress,
        }
    }

    fn build(&self) -> Filters {
        Filters {
            ingress: self.ingress.build(),
            deliver: self.deliver.build(),
            originate: self.originate.build(),
            egress: self.egress.build(),
        }
    }
}

pub struct FilterEngine {
    builders: Mutex<Builders>,
    /// Lock-free access to the current built filter chains.
    filters: ArcSwap<Filters>,
}

impl FilterEngine {
    pub fn new() -> Self {
        let builders = Mutex::new(Builders::default());
        let filters = ArcSwap::from_pointee(Filters::default());

        Self { builders, filters }
    }

    pub fn clear(&self) {
        let builders = Builders::default();
        self.filters.store(Arc::new(builders.build()));
        *self.builders.lock() = builders;
    }

    pub fn register(
        &self,
        hook: Hook,
        name: &str,
        after: &[&str],
        filter: Filter,
    ) -> Result<(), Error> {
        let mut builders = self.builders.lock();
        builders.get_mut(&hook).add_filter(name, filter, after)?;
        self.filters.store(Arc::new(builders.build()));

        metrics::gauge!("bpa.filter.registered", "hook" => hook.label()).increment(1.0);
        Ok(())
    }

    pub fn unregister(&self, hook: Hook, name: &str) -> Result<Option<Filter>, Error> {
        let mut builders = self.builders.lock();
        let result = builders.get_mut(&hook).remove_filter(name)?;

        if result.is_some() {
            self.filters.store(Arc::new(builders.build()));
            metrics::gauge!("bpa.filter.registered", "hook" => hook.label()).decrement(1.0);
        }

        Ok(result)
    }

    /// Load the current filters lock-free, then execute.
    pub async fn exec(
        &self,
        hook: Hook,
        bundle: Bundle,
        data: Bytes,
        key_store: &Arc<KeyStore>,
        pool: &hardy_async::BoundedTaskPool,
    ) -> Result<ExecResult, crate::Error> {
        let hook_label = hook.label();
        let filters = self.filters.load();
        let result = filters
            .chain(&hook)
            .exec(pool, bundle, data, key_store)
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
