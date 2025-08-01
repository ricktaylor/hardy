use super::*;

impl Dispatcher {
    pub(super) async fn fragment(
        &self,
        _max_bundle_size: u64,
        _bundle: &bundle::Bundle,
    ) -> Result<dispatch::DispatchResult, Error> {
        warn!("Bundle requires fragmentation");
        todo!()
    }

    pub(super) async fn reassemble(
        &self,
        _bundle: &bundle::Bundle,
    ) -> Result<dispatch::DispatchResult, Error> {
        /* Either wait for more fragments to arrive
        self.store.set_status(&mut bundle, BundleStatus::ReassemblyPending).await?;

        Or

        // TODO: We need to handle the case when the reassembled fragment is larger than our total RAM!
        Reassemble and self.enqueue_bundle()

        */

        warn!("Bundle requires fragment reassembly");
        todo!()
    }
}
