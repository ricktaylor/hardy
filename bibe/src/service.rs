use super::*;
use hardy_bpa::services::{Service, ServiceSink, StatusNotify};

/// BIBE Decapsulation Service.
///
/// Receives outer bundles, extracts the inner bundle from the payload,
/// and re-injects it into the BPA via the CLA's dispatch method.
pub struct DecapService {
    cla: Arc<cla::BibeCla>,
    sink: Once<Box<dyn ServiceSink>>,
}

impl DecapService {
    /// Create a new DecapService using the given CLA for dispatch.
    pub fn new(cla: Arc<cla::BibeCla>) -> Self {
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

    /// Extract inner bundle from outer bundle payload.
    fn decapsulate(&self, outer_bytes: Bytes) -> Result<Bytes, Error> {
        // Structural parse — we only need the payload block range; no
        // BPSec validation is required to decapsulate.
        let hardy_bpv7::parse::Parsed {
            data: outer_bytes,
            bundle: parsed_bundle,
            ..
        } = hardy_bpv7::parse::parse(outer_bytes)?;

        // Get payload block (block number 1) and its range within outer_bytes
        let payload_block = parsed_bundle
            .blocks
            .get(&1)
            .ok_or(hardy_bpv7::Error::MissingBlock(1))?;
        // Payload is BIBE-PDU: [transmission-id, total-length, segmented-offset, bundle-segment]
        // For complete bundles: all three ints are 0. `payload` bounds-checks the
        // wire-derived range (returns None on a 32-bit-unrepresentable or
        // over-claiming extent) instead of slicing with a truncating `as usize`.
        let payload = payload_block
            .payload(&outer_bytes)
            .map(|p| outer_bytes.slice_ref(p))
            .ok_or(hardy_bpv7::Error::MissingBlock(1))?;
        let (inner_range, len) = hardy_cbor::decode::parse_array(
            &payload,
            |a, _shortest, _tags| -> Result<_, hardy_cbor::decode::Error> {
                let _transmission_id: u64 = a.parse()?;
                let _total_length: u64 = a.parse()?;
                let _segmented_offset: u64 = a.parse()?;
                // Parse byte string and get the range within payload
                a.parse_value(|value, _shortest, _tags| match value {
                    hardy_cbor::decode::Value::Bytes(range) => Ok(range),
                    _ => Err(hardy_cbor::decode::Error::IncorrectType(
                        "Byte String".into(),
                        value.type_name(false),
                    )),
                })
            },
        )?;

        // Check for smuggled data after the CBOR array
        if len != payload.len() {
            return Err(hardy_cbor::decode::Error::AdditionalItems.into());
        }

        // Return zero-copy slice of the inner bundle
        Ok(payload.slice(inner_range))
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

    async fn on_receive(
        &self,
        data: Bytes,
        _expiry: time::OffsetDateTime,
    ) -> hardy_bpa::services::Result<()> {
        // A malformed outer bundle is a permanent failure: log and accept it,
        // so it is not parked for a retry that could never succeed.
        let inner = match self.decapsulate(data) {
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
            .map_err(|e| hardy_bpa::services::Error::Internal(e.into()))
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: hardy_bpv7::status_report::ReasonCode,
        _timestamp: Option<time::OffsetDateTime>,
    ) {
        // DecapService doesn't send bundles, so no status reports expected
    }
}
