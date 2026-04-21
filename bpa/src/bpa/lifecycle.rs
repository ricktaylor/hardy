#[cfg(feature = "instrument")]
use tracing::instrument;

use super::Bpa;
use crate::otel_metrics;
use crate::recover::Recovery;

impl Bpa {
    /// Reconcile storage after an unclean shutdown.
    ///
    /// Spawns the three-phase recovery protocol as a background task.
    /// Call before [`start()`](Bpa::start).
    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn recover(&self) {
        let store = self.store.clone();
        let dispatcher = self.dispatcher.clone();
        hardy_async::spawn!(self.store.tasks(), "recovery", async move {
            Recovery::new(&store, &dispatcher)
                .mark()
                .await
                .reconcile()
                .await
                .purge()
                .await;
        });
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub fn start(&self) {
        otel_metrics::init();
        self.store.start(self.dispatcher.clone());
        self.rib.start(self.dispatcher.clone());
    }

    #[cfg_attr(feature = "instrument", instrument(skip(self)))]
    pub async fn shutdown(&self) {
        // Shutdown order is critical for clean termination:
        //
        // 1. Routing agents - Remove dynamic routes (prevents new forwarding decisions)
        // 2. CLAs - Stop external bundle sources (network I/O)
        // 3. Services - Stop internal bundle sources (applications calling sink.send())
        // 4. Dispatcher - Drain remaining in-flight bundles (all sources now closed)
        // 5. RIB - No more routing lookups needed
        // 6. Store - No more data access needed
        //
        // Routing agents shut down first so their routes are removed before CLAs
        // drain. CLAs and Services must shut down BEFORE dispatcher because they
        // are bundle sources. The dispatcher's processing pool may have tasks
        // blocked on CLA forwarding or waiting for service responses.

        self.rib.shutdown_agents().await;
        self.cla_registry.shutdown().await;
        self.service_registry
            .shutdown(&self.node_ids, &self.rib)
            .await;
        self.dispatcher.shutdown().await;
        self.rib.shutdown().await;
        self.store.shutdown().await;
        self.filter_registry.clear();
    }
}
