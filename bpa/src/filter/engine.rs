use arc_swap::ArcSwap;
use hardy_async::sync::Mutex;
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
    /// Dedicated unbounded pool for parallel ReadFilter execution, shared by
    /// all chains. Deliberately NOT the dispatcher's processing pool:
    /// exec() callers hold processing-pool permits (process_bundle →
    /// reassembly/delivery → exec), and spawning filter tasks onto that pool
    /// self-deadlocks once it saturates. Unbounded is safe here because
    /// every spawned task is awaited within exec(), so concurrency is
    /// transitively bounded by the callers and the pool is idle between
    /// calls — and with no semaphore, no permit cycle can form at all.
    pool: hardy_async::TaskPool,
}

impl FilterEngine {
    pub fn new() -> Self {
        let builders = Mutex::new(Builders::default());
        let filters = ArcSwap::from_pointee(Filters::default());

        Self {
            builders,
            filters,
            pool: hardy_async::TaskPool::new(),
        }
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
    pub async fn exec<F>(
        &self,
        hook: Hook,
        bundle: Bundle,
        data: Bytes,
        key_provider: F,
    ) -> Result<ExecResult, crate::Error>
    where
        F: Fn(&Bpv7Bundle, &[u8]) -> Box<dyn KeySource> + Clone + Send,
    {
        let hook_label = hook.label();
        let filters = self.filters.load();
        let result = filters
            .chain(&hook)
            .exec(&self.pool, bundle, data, key_provider)
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

#[cfg(test)]
mod tests {
    use hardy_async::async_trait;

    use super::*;
    use crate::filter::ReadResult;

    struct PassFilter;

    #[async_trait]
    impl crate::filter::ReadFilter for PassFilter {
        async fn filter(&self, _bundle: &Bundle, _data: &[u8]) -> Result<ReadResult, crate::Error> {
            Ok(ReadResult::Continue)
        }
    }

    // exec() must complete even when every caller holds a permit of a
    // saturated BoundedTaskPool: filter tasks run on the engine's own
    // dedicated pool, never the callers'. Spawning them on the callers'
    // pool deadlocks this exact arrangement (all permits held by tasks
    // parked waiting for another permit).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exec_completes_from_saturated_caller_pool() {
        let engine = Arc::new(FilterEngine::new());
        engine
            .register(Hook::Ingress, "a", &[], Filter::Read(Arc::new(PassFilter)))
            .unwrap();
        engine
            .register(Hook::Ingress, "b", &[], Filter::Read(Arc::new(PassFilter)))
            .unwrap();

        let caller_pool =
            hardy_async::BoundedTaskPool::new(core::num::NonZeroUsize::new(2).unwrap());

        let mut handles = Vec::new();
        for _ in 0..2 {
            let engine = engine.clone();
            handles.push(
                hardy_async::spawn!(caller_pool, "outer_task", async move {
                    let bundle = Bundle {
                        bundle: Default::default(),
                        metadata: Default::default(),
                    };
                    let result = engine
                        .exec(
                            Hook::Ingress,
                            bundle,
                            Bytes::new(),
                            hardy_bpv7::bpsec::no_keys,
                        )
                        .await
                        .unwrap();
                    assert!(matches!(result, ExecResult::Continue(..)));
                })
                .await,
            );
        }

        tokio::time::timeout(core::time::Duration::from_secs(5), async {
            for handle in handles {
                handle.await.unwrap();
            }
        })
        .await
        .expect("exec() deadlocked while callers held all pool permits");
    }
}
