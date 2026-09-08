use alloc::{boxed::Box, sync::Arc};

use hardy_async::sync::spin::Once;
use hardy_bpa::{
    async_trait,
    services::{
        Error as ServiceError, Result as ServiceResult, Service, ServiceSink, StatusNotify,
    },
    stream::{Receiver, Segment, buffer_stream},
};
use hardy_bpv7::{bundle::Id, eid::Eid, status_report::ReasonCode};
use time::OffsetDateTime;
use tracing::{debug, warn};

use crate::{cla::BibeCla, pdu};

/// BIBE Decapsulation Service.
///
/// Receives outer bundles, extracts the inner bundle from the payload,
/// and re-injects it into the BPA via the CLA's dispatch method.
pub struct DecapService {
    cla: Arc<BibeCla>,
    sink: Once<Box<dyn ServiceSink>>,
}

impl DecapService {
    /// Create a new DecapService using the given CLA for dispatch.
    pub fn new(cla: Arc<BibeCla>) -> Self {
        Self {
            cla,
            sink: Once::new(),
        }
    }

    /// Unregister this service from the BPA.
    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl Service for DecapService {
    async fn on_register(&self, endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        self.sink.call_once(|| sink);
        debug!("BIBE DecapService registered at {endpoint}");
    }

    async fn on_unregister(&self) {
        debug!("BIBE DecapService unregistered");
    }

    // INTERIM BUFFERING: decapsulation parses the whole outer bundle with a
    // whole-buffer codec, so the stream is assembled in memory via
    // `stream::buffer_stream` first. This is a deliberate stepping stone
    // toward the full streaming pipeline; see
    // bpa/docs/streaming_pipeline_design.md.
    async fn on_deliver(
        &self,
        _bundle_id: &Id,
        _expiry: OffsetDateTime,
        total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> ServiceResult<()> {
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

        // A dispatch failure is transient: propagate it so the outer bundle is
        // parked and retried rather than dropped.
        debug!("BIBE decapsulated bundle, dispatching");
        self.cla
            .dispatch(inner)
            .await
            .inspect_err(|e| warn!("Failed to dispatch decapsulated bundle: {e}"))
            .map_err(|e| ServiceError::Internal(e.into()))
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
        // DecapService doesn't send bundles, so no status reports expected
    }
}
