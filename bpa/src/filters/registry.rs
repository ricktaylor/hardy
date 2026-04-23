use hardy_async::sync::RwLock;
use hardy_bpv7::bpsec::key::KeySource;
use hardy_bpv7::bundle::Bundle as Bpv7Bundle;

use super::chain::FilterChain;
use super::{Error, ExecResult, Filter, Hook};
use crate::Bytes;
use crate::bundle::Bundle;

#[derive(Default)]
struct RegistryInner {
    ingress: FilterChain,
    deliver: FilterChain,
    originate: FilterChain,
    egress: FilterChain,
}

impl RegistryInner {
    fn chain(&self, hook: &Hook) -> &FilterChain {
        match hook {
            Hook::Ingress => &self.ingress,
            Hook::Deliver => &self.deliver,
            Hook::Originate => &self.originate,
            Hook::Egress => &self.egress,
        }
    }

    fn chain_mut(&mut self, hook: &Hook) -> &mut FilterChain {
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
        *self.inner.write() = RegistryInner::default();
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
        bundle: Bundle,
        data: Bytes,
        key_provider: F,
        pool: &hardy_async::BoundedTaskPool,
    ) -> Result<ExecResult, crate::Error>
    where
        F: Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource> + Clone + Send,
    {
        let hook_label = hook.label();
        let prepared = self.inner.read().chain(&hook).prepare();
        let result = prepared.exec(pool, bundle, data, key_provider).await;

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
