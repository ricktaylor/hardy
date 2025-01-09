use super::*;

impl Dispatcher {
    #[instrument(skip(self))]
    pub(super) async fn reassemble(
        &self,
        _bundle: &mut bundle::Bundle,
    ) -> Result<DispatchResult, Error> {
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
