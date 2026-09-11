use alloc::borrow::Cow;

use hardy_bpa::Bytes;
use hardy_bpv7::{
    CreationTimestamp, Error as Bpv7Error,
    builder::Builder,
    eid::Eid,
    parser::{Parsed, parse},
};
use hardy_cbor::{
    decode::{Error as CborError, Value, parse_array},
    encode::{Bytes as CborBytes, emit_array},
};

use crate::Error;

/// Wrap a complete inner bundle in an outer bundle carrying a BIBE-PDU payload.
///
/// The outer bundle is sourced at `tunnel_source`, addressed to `outer_dest`,
/// and inherits the inner bundle's lifetime.
pub fn encapsulate(tunnel_source: &Eid, inner: Bytes, outer_dest: Eid) -> Result<Bytes, Error> {
    // Parse inner bundle structurally to read its lifetime.
    let Parsed {
        data: inner,
        bundle: parsed_bundle,
        ..
    } = parse(inner)?;
    let lifetime = parsed_bundle.primary.lifetime;

    // Build outer bundle with BIBE-PDU payload:
    // [transmission-id, total-length, segmented-offset, encapsulated-bundle-segment]
    // For complete bundles: [0, 0, 0, bundle-bytes]
    let payload = emit_array(Some(4), |a| {
        a.emit(&0u64); // transmission-id
        a.emit(&0u64); // total-length
        a.emit(&0u64); // segmented-offset

        // encapsulated-bundle-segment, as a definite-length byte string
        // (a bare `&[u8]` would encode as a CBOR array of integers, which
        // decapsulation rejects)
        a.emit(&CborBytes(inner.as_ref()));
    });

    let (_bundle, data) = Builder::new(tunnel_source.clone(), outer_dest)
        .with_lifetime(lifetime)
        .with_payload(Cow::Owned(payload))
        .build(CreationTimestamp::now())?;

    Ok(data.into())
}

/// Extract the inner bundle from an outer bundle's BIBE-PDU payload.
pub fn decapsulate(outer_bytes: Bytes) -> Result<Bytes, Error> {
    // Structural parse — we only need the payload block range; no BPSec
    // validation is required to decapsulate.
    let Parsed {
        data: outer_bytes,
        bundle: parsed_bundle,
        ..
    } = parse(outer_bytes)?;

    // Get payload block (block number 1) and its range within outer_bytes
    let payload_block = parsed_bundle
        .blocks
        .get(&1)
        .ok_or(Bpv7Error::MissingBlock(1))?;

    // Payload is BIBE-PDU: [transmission-id, total-length, segmented-offset, bundle-segment]
    // For complete bundles: all three ints are 0. `payload` bounds-checks the
    // wire-derived range (returns None on a 32-bit-unrepresentable or
    // over-claiming extent) instead of slicing with a truncating `as usize`.
    let payload = payload_block
        .payload(&outer_bytes)
        .map(|p| outer_bytes.slice_ref(p))
        .ok_or(Bpv7Error::MissingBlock(1))?;
    let (inner_range, len) = parse_array(&payload, |a, _shortest, _tags| -> Result<_, Error> {
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
        a.parse_value(|value, _shortest, tags| match value {
            Value::Bytes(range) => Ok(segment_start + range.start..segment_start + range.end),
            _ => Err(CborError::IncorrectType(
                "Byte String",
                value.item_type(tags),
            )),
        })
        .map_err(Into::into)
    })?;

    // Check for smuggled data after the CBOR array
    if len != payload.len() {
        return Err(CborError::AdditionalItems.into());
    }

    // Return zero-copy slice of the inner bundle
    Ok(payload.slice(inner_range))
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    const INNER_LIFETIME: core::time::Duration = core::time::Duration::from_secs(60);

    const TUNNEL_SOURCE: &str = "ipn:10.1";
    const DECAP_ENDPOINT: &str = "ipn:20.5";

    // Build a complete inner bundle with a payload of the given length.
    fn make_inner(payload_len: usize) -> Bytes {
        let (_, data) = Builder::new("ipn:1.1".parse().unwrap(), "ipn:2.1".parse().unwrap())
            .with_lifetime(INNER_LIFETIME)
            .with_payload(Cow::Owned(vec![0x5A; payload_len]))
            .build(CreationTimestamp::now())
            .unwrap();
        Bytes::from(data)
    }

    // Build an outer bundle carrying an arbitrary byte sequence as its payload,
    // standing in for a (possibly malformed) BIBE-PDU.
    fn make_outer(payload: Vec<u8>) -> Bytes {
        let (_, data) = Builder::new(
            TUNNEL_SOURCE.parse().unwrap(),
            DECAP_ENDPOINT.parse().unwrap(),
        )
        .with_lifetime(INNER_LIFETIME)
        .with_payload(Cow::Owned(payload))
        .build(CreationTimestamp::now())
        .unwrap();
        Bytes::from(data)
    }

    // The full BIBE-PDU wire format: encapsulate then decapsulate must return
    // the inner bundle byte-identically. Inner sizes straddle the CBOR
    // byte-string length-field boundaries (23/24, 255/256, 65535/65536 payload
    // bytes) to catch length-encoding splice and range off-by-one errors.
    #[test]
    fn test_encap_decap_round_trip() {
        let tunnel_source: Eid = TUNNEL_SOURCE.parse().unwrap();
        let decap_endpoint: Eid = DECAP_ENDPOINT.parse().unwrap();

        for payload_len in [0usize, 1, 23, 24, 255, 256, 65535, 65536] {
            let inner = make_inner(payload_len);
            let outer = encapsulate(&tunnel_source, inner.clone(), decap_endpoint.clone()).unwrap();

            let parsed = parse(outer.clone()).unwrap();
            assert_eq!(parsed.bundle.primary.destination, decap_endpoint);
            assert_eq!(parsed.bundle.primary.id.source, tunnel_source);
            assert_eq!(parsed.bundle.primary.lifetime, INNER_LIFETIME);

            let recovered = decapsulate(outer).unwrap();
            assert_eq!(
                recovered.as_ref(),
                inner.as_ref(),
                "inner bundle must round-trip byte-identically (payload_len {payload_len})"
            );
            parse(recovered).unwrap();
        }
    }

    // A BIBE-PDU with any non-zero transmission-id/total-length/segmented-offset
    // is a segment of a larger bundle; decapsulation must reject it rather than
    // dispatch the segment as a complete bundle.
    #[test]
    fn test_decap_rejects_segment() {
        for (transmission_id, total_length, segmented_offset) in
            [(1u64, 0u64, 0u64), (0, 100, 0), (0, 0, 50)]
        {
            let pdu = emit_array(Some(4), |a| {
                a.emit(&transmission_id);
                a.emit(&total_length);
                a.emit(&segmented_offset);
                a.emit(&CborBytes(b"partial-bundle-bytes".as_slice()));
            });

            let err = decapsulate(make_outer(pdu)).unwrap_err();
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
        let mut pdu = emit_array(Some(4), |a| {
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&CborBytes(inner.as_ref()));
        });
        pdu.extend_from_slice(b"garbage");

        let err = decapsulate(make_outer(pdu)).unwrap_err();
        assert!(
            matches!(err, Error::Cbor(CborError::AdditionalItems)),
            "expected AdditionalItems, got {err:?}"
        );
    }

    // The encapsulated-bundle-segment element must be a byte string.
    #[test]
    fn test_decap_wrong_segment_type() {
        let pdu = emit_array(Some(4), |a| {
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit("not a byte string");
        });

        let err = decapsulate(make_outer(pdu)).unwrap_err();
        assert!(
            matches!(err, Error::Cbor(CborError::IncorrectType(_, _))),
            "expected IncorrectType, got {err:?}"
        );
    }

    // A PDU array with fewer than four elements is malformed.
    #[test]
    fn test_decap_short_array() {
        let pdu = emit_array(Some(3), |a| {
            a.emit(&0u64);
            a.emit(&0u64);
            a.emit(&0u64);
        });

        assert!(decapsulate(make_outer(pdu)).is_err());
    }
}
