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

    /// Extract inner bundle from outer bundle payload.
    fn decapsulate(&self, outer_bytes: Bytes) -> Result<Bytes, Error> {
        // Parse the outer bundle
        let parsed = ParsedBundle::parse(&outer_bytes, bpsec::no_keys)?;

        // Get payload block (block number 1) and its range within outer_bytes
        let payload_block = parsed
            .bundle
            .blocks
            .get(&1)
            .ok_or(hardy_bpv7::Error::MissingBlock(1))?;
        let payload_range = payload_block.payload_range();

        // Payload is BIBE-PDU: [transmission-id, total-length, segmented-offset, bundle-segment]
        // For complete bundles: all three ints are 0
        let payload = outer_bytes.slice(payload_range);
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

    async fn on_receive(&self, data: Bytes, _expiry: time::OffsetDateTime) {
        match self.decapsulate(data) {
            Ok(inner) => {
                debug!("BIBE decapsulated bundle, dispatching");
                if let Err(e) = self.cla.dispatch(inner).await {
                    error!("Failed to dispatch decapsulated bundle: {e}");
                }
            }
            Err(e) => {
                error!("BIBE decapsulation failed: {e}");
            }
        }
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
