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
    pub fn decapsulate(&self, outer_bytes: Bytes) -> Result<Bytes, Error> {
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
        let (inner_range, len) =
            hardy_cbor::decode::parse_array(&payload, |a, _shortest, _tags| -> Result<_, Error> {
                let transmission_id: u64 = a.parse()?;
                let total_length: u64 = a.parse()?;
                let segmented_offset: u64 = a.parse()?;

                // A complete bundle carries all three fields as zero; anything
                // else marks a segment of a larger bundle, and this
                // implementation does not reassemble segments, so dispatching
                // the segment as a complete bundle would inject garbage.
                if transmission_id != 0 || total_length != 0 || segmented_offset != 0 {
                    return Err(Error::SegmentedPdu);
                }

                // Parse the byte string and get its range within payload. The
                // range reported by `parse_value` is relative to the start of
                // the item, so rebase it onto the item's offset within the
                // payload before slicing.
                let segment_start = a.offset();
                a.parse_value(|value, _shortest, _tags| match value {
                    hardy_cbor::decode::Value::Bytes(range) => {
                        Ok(segment_start + range.start..segment_start + range.end)
                    }
                    _ => Err(hardy_cbor::decode::Error::IncorrectType(
                        "Byte String".into(),
                        value.type_name(false),
                    )),
                })
                .map_err(Into::into)
            })?;

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

    // INTERIM BUFFERING: decapsulation parses the whole outer bundle with a
    // whole-buffer codec, so the stream is assembled in memory via
    // `stream::buffer_stream` first. This is a deliberate stepping stone
    // toward the full streaming pipeline; see
    // bpa/docs/streaming_pipeline_design.md.
    async fn on_deliver(
        &self,
        _bundle_id: &hardy_bpv7::bundle::Id,
        _expiry: time::OffsetDateTime,
        total_len: u64,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::services::Result<()> {
        let data = hardy_bpa::stream::buffer_stream(stream, total_len).await?;

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

#[cfg(test)]
mod tests {
    use hardy_bpv7::builder::Builder;

    use super::*;

    const INNER_LIFETIME: core::time::Duration = core::time::Duration::from_secs(60);

    // Build a complete inner bundle with a payload of the given length.
    fn make_inner(payload_len: usize) -> Bytes {
        let (_, data) = Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_lifetime(INNER_LIFETIME)
            .with_payload(Cow::Owned(alloc::vec![0x5A; payload_len]))
            .build(CreationTimestamp::now())
            .unwrap();
        Bytes::from(data)
    }

    // Build an outer bundle carrying an arbitrary byte sequence as its payload,
    // standing in for a (possibly malformed) BIBE-PDU.
    fn make_outer(payload: Vec<u8>) -> Bytes {
        let (_, data) = Builder::new("ipn:10.1".parse().unwrap(), "ipn:20.5".parse().unwrap())
            .with_lifetime(INNER_LIFETIME)
            .with_payload(Cow::Owned(payload))
            .build(CreationTimestamp::now())
            .unwrap();
        Bytes::from(data)
    }

    fn make_decap() -> DecapService {
        DecapService::new(Arc::new(cla::BibeCla::new("ipn:10.1".parse().unwrap())))
    }

    // The full BIBE-PDU wire format: encapsulate then decapsulate must return
    // the inner bundle byte-identically. Inner sizes straddle the CBOR
    // byte-string length-field boundaries (23/24, 255/256, 65535/65536 payload
    // bytes) to catch length-encoding splice and range off-by-one errors.
    #[test]
    fn test_encap_decap_round_trip() {
        let tunnel_source: Eid = "ipn:10.1".parse().unwrap();
        let decap_endpoint: Eid = "ipn:20.5".parse().unwrap();
        let bibe_cla = cla::BibeCla::new(tunnel_source.clone());
        let decap = make_decap();

        for payload_len in [0usize, 1, 23, 24, 255, 256, 65535, 65536] {
            let inner = make_inner(payload_len);
            let outer = bibe_cla
                .encapsulate(inner.clone(), decap_endpoint.clone())
                .unwrap();

            let parsed = ParsedBundle::parse(&outer, bpsec::no_keys).unwrap();
            assert_eq!(parsed.bundle.destination, decap_endpoint);
            assert_eq!(parsed.bundle.id.source, tunnel_source);
            assert_eq!(parsed.bundle.lifetime, INNER_LIFETIME);

            let recovered = decap.decapsulate(outer).unwrap();
            assert_eq!(
                recovered.as_ref(),
                inner.as_ref(),
                "inner bundle must round-trip byte-identically (payload_len {payload_len})"
            );
            ParsedBundle::parse(&recovered, bpsec::no_keys).unwrap();
        }
    }

    // A BIBE-PDU with any non-zero transmission-id/total-length/segmented-offset
    // is a segment of a larger bundle; decapsulation must reject it rather than
    // dispatch the segment as a complete bundle.
    #[test]
    fn test_decap_rejects_segment() {
        let decap = make_decap();

        for (transmission_id, total_length, segmented_offset) in
            [(1u64, 0u64, 0u64), (0, 100, 0), (0, 0, 50)]
        {
            let pdu = hardy_cbor::encode::emit_array(Some(4), |a| {
                a.emit(&transmission_id);
                a.emit(&total_length);
                a.emit(&segmented_offset);
                a.emit(&hardy_cbor::encode::Bytes(
                    b"partial-bundle-bytes".as_slice(),
                ));
            });

            let err = decap.decapsulate(make_outer(pdu)).unwrap_err();
            assert!(
                matches!(err, Error::SegmentedPdu),
                "expected SegmentedPdu for ({transmission_id}, {total_length}, {segmented_offset}), got {err:?}"
            );
        }
    }

    // Bytes smuggled after the BIBE-PDU array must be rejected.
    #[test]
    fn test_decap_trailing_garbage() {
        let inner = make_inner(4);
        let mut pdu = hardy_cbor::encode::emit_array(Some(4), |a| {
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&hardy_cbor::encode::Bytes(inner.as_ref()));
        });
        pdu.extend_from_slice(b"garbage");

        let err = make_decap().decapsulate(make_outer(pdu)).unwrap_err();
        assert!(
            matches!(err, Error::Cbor(hardy_cbor::decode::Error::AdditionalItems)),
            "expected AdditionalItems, got {err:?}"
        );
    }

    // The encapsulated-bundle-segment element must be a byte string.
    #[test]
    fn test_decap_wrong_segment_type() {
        let pdu = hardy_cbor::encode::emit_array(Some(4), |a| {
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit("not a byte string");
        });

        let err = make_decap().decapsulate(make_outer(pdu)).unwrap_err();
        assert!(
            matches!(
                err,
                Error::Cbor(hardy_cbor::decode::Error::IncorrectType(_, _))
            ),
            "expected IncorrectType, got {err:?}"
        );
    }

    // A PDU array with fewer than four elements is malformed.
    #[test]
    fn test_decap_short_array() {
        let pdu = hardy_cbor::encode::emit_array(Some(3), |a| {
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&0u64);
        });

        assert!(make_decap().decapsulate(make_outer(pdu)).is_err());
    }
}
