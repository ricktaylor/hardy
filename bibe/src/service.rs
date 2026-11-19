use alloc::sync::Arc;

use hardy_bpa::{
    async_trait,
    cla::Acceptance,
    service::{self, StatusNotify, raw},
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use time::OffsetDateTime;
use tracing::{debug, warn};

use crate::{cla::BibeCla, pdu};

/// The decapsulating half of a BIBE tunnel.
///
/// Each outer bundle delivered here yields the inner bundle carried in its
/// payload, which the CLA dispatches back into the BPA.
pub struct DecapService {
    cla: Arc<BibeCla>,

    /// Communication channel back to the BPA, for the service's whole
    /// active lifetime.
    sink: Arc<dyn raw::Sink>,
}

impl DecapService {
    /// A factory for a decapsulation service that re-injects the inner
    /// bundles it extracts through `cla`.
    pub fn factory(cla: Arc<BibeCla>) -> impl raw::Factory<Component = Self> {
        raw::factory_fn(move |cx: raw::Context| Ok(Arc::new(Self { cla, sink: cx.sink })))
    }

    /// Unregisters the service from the BPA.
    pub async fn unregister(&self) {
        self.sink.unregister().await;
    }
}

#[async_trait]
impl raw::Service for DecapService {
    // Decapsulation parses the whole outer bundle with a whole-buffer
    // codec, so `stream::buffer_stream` assembles the stream in memory
    // first. See bpa/docs/streaming_pipeline_design.md for the streaming
    // form this is a step towards.
    async fn deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> service::Result<()> {
        let data = buffer_stream(stream, total_len).await?;

        // A malformed outer bundle is a permanent failure: log and accept it,
        // so it is not parked for a retry that could never succeed.
        let inner = match pdu::decapsulate(data) {
            Ok(inner) => inner,
            Err(e) => {
                warn!("BIBE decapsulation failed: {e}");
                return Ok(());
            }
        };

        debug!("BIBE decapsulated bundle, dispatching");
        match self.cla.dispatch(inner).await {
            Ok(Acceptance::Accepted) => Ok(()),
            // A refusal is deterministic for these bytes, the inner bundle
            // being unable to outgrow the outer, so this is no size race:
            // accept the outer bundle rather than park it for a retry that
            // could never succeed, as for a malformed outer.
            Ok(Acceptance::Refused) => {
                warn!("Decapsulated bundle refused by the BPA, outer bundle consumed");
                Ok(())
            }
            // A dispatch fault is transient: propagate it so the outer
            // bundle is parked and retried rather than dropped.
            Err(e) => {
                warn!("Failed to dispatch decapsulated bundle: {e}");
                Err(service::Error::Internal(e.into()))
            }
        }
    }

    /// Discards the notification: the service sends no bundles of its own.
    async fn report_status(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}
