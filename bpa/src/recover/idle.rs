use tracing::info;

use super::{Idle, Marked, Recovery};

impl<'a> Recovery<'a, Idle> {
    pub(crate) fn new(
        store: &'a crate::Arc<crate::storage::Store>,
        dispatcher: &'a crate::Arc<crate::dispatcher::Dispatcher>,
    ) -> Self {
        Self {
            store,
            dispatcher,
            _state: core::marker::PhantomData,
        }
    }

    /// Phase 1: Mark all metadata entries as unconfirmed.
    pub(crate) async fn mark(self) -> Recovery<'a, Marked> {
        info!("Starting store consistency check...");
        self.store.mark_unconfirmed().await;
        self.transition()
    }
}
