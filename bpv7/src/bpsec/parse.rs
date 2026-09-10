use super::*;

use alloc::{boxed::Box, sync::Arc};
use core::ops::Range;

use hardy_cbor::decode::Untagged;
use smallvec::SmallVec;

use crate::{HashMap, canonical::require_canonical, eid};

/// Strict-canonical helper per RFC 9172 §4 — no §4.1 carveout for ASB
/// content, so every encoding violation (non-shortest, indefinite-
/// length, unexpected tags) is rejected with `NotCanonical`.
fn parse_ranges<const D: usize>(
    seq: &mut hardy_cbor::decode::Series<D>,
    mut offset: usize,
) -> Result<Option<HashMap<u64, Range<usize>>>> {
    if seq.at_end()? {
        return Ok(None);
    }

    offset += seq.offset();
    seq.parse_array(|a, s, tags| {
        if !s || !tags.is_empty() || !a.is_definite() {
            return Err(Error::NotCanonical);
        }
        let mut outer_offset = a.offset();

        let mut map = HashMap::new();
        while !a.at_end()? {
            let (id, r) = a.parse_array(|a, s, tags| {
                if !s || !tags.is_empty() || !a.is_definite() {
                    return Err(Error::NotCanonical);
                }

                let id = require_canonical(a, "id", Error::NotCanonical)?;
                let data_start = offset + outer_offset + a.offset();
                a.skip_value(16).map_field_err::<Error>("value")?;
                Ok::<_, Error>((id, data_start..offset + outer_offset + a.offset()))
            })?;
            map.insert(id, r);
            outer_offset = a.offset();
        }
        Ok(Some(map))
    })
}

#[derive(Debug)]
pub struct UnknownOperation {
    pub parameters: Arc<HashMap<u64, Box<[u8]>>>,
    pub results: HashMap<u64, Box<[u8]>>,
}

/// Bounds-checked slice into a BPSec-related `source_data` buffer.
///
/// Every parameter/result range stored in an [`AbstractSyntaxBlock`]
/// originally came from parsing `source_data`, so under normal use the
/// range is in-bounds. The check guards against a caller passing a
/// partial slice (early-block-processing case) or a mismatched buffer —
/// it converts a release-mode panic into a clean [`Error::SourceOutOfRange`].
pub(super) fn bounded_slice(data: &[u8], range: Range<usize>) -> Result<&[u8]> {
    data.get(range.clone()).ok_or(Error::SourceOutOfRange {
        start: range.start,
        end: range.end,
        source_len: data.len(),
    })
}

// Copies each range of `data` into an owned buffer, keyed as on the wire.
fn slice_map(ranges: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<HashMap<u64, Box<[u8]>>> {
    let mut map = HashMap::with_capacity(ranges.len());
    for (id, range) in ranges {
        map.insert(id, bounded_slice(data, range)?.into());
    }
    Ok(map)
}

impl UnknownOperation {
    pub fn parse(
        asb: AbstractSyntaxBlock,
        source_data: &[u8],
    ) -> Result<(eid::Eid, HashMap<u64, Self>)> {
        asb.into_operations(
            source_data,
            "security context parameters",
            "security results",
            slice_map,
            slice_map,
            |parameters, results| Self {
                parameters,
                results,
            },
        )
    }

    pub fn emit_context(
        &self,
        encoder: &mut hardy_cbor::encode::Encoder,
        source: &eid::Eid,
        id: u64,
    ) {
        encoder.emit(&id);
        if self.parameters.is_empty() {
            encoder.emit(&0);
            encoder.emit(source);
        } else {
            encoder.emit(&1);
            encoder.emit(source);
            encoder.emit_array(Some(self.parameters.len()), |a| {
                for (id, result) in self.parameters.iter() {
                    a.emit(&(id, hardy_cbor::encode::Raw(result)));
                }
            });
        }
    }

    pub fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        array.emit_array(Some(self.results.len()), |a| {
            for (id, result) in &self.results {
                a.emit(&(id, hardy_cbor::encode::Raw(result)));
            }
        });
    }
}

pub struct AbstractSyntaxBlock {
    pub context: Context,
    // Private: the ranges can only be consumed through `into_operations`,
    // so every security context unpacks an ASB the same way.
    source: eid::Eid,
    parameters: HashMap<u64, Range<usize>>,
    results: HashMap<u64, HashMap<u64, Range<usize>>>,
}

impl AbstractSyntaxBlock {
    /// The only way to consume the parameter/result ranges: parses the
    /// parameters once, shares them across every target, parses one
    /// result set per target keyed by its wire target number, and wraps
    /// each failure in `InvalidField` with the given field name.
    pub fn into_operations<P, R, Op>(
        self,
        data: &[u8],
        params_field: &'static str,
        results_field: &'static str,
        parse_params: impl FnOnce(HashMap<u64, Range<usize>>, &[u8]) -> Result<P>,
        parse_results: impl Fn(HashMap<u64, Range<usize>>, &[u8]) -> Result<R>,
        make: impl Fn(Arc<P>, R) -> Op,
    ) -> Result<(eid::Eid, HashMap<u64, Op>)> {
        let parameters =
            Arc::new(parse_params(self.parameters, data).map_field_err::<Error>(params_field)?);
        let mut operations = HashMap::with_capacity(self.results.len());
        for (target, results) in self.results {
            let results = parse_results(results, data).map_field_err::<Error>(results_field)?;
            operations.insert(target, make(parameters.clone(), results));
        }
        Ok((self.source, operations))
    }
}

impl hardy_cbor::decode::FromCbor for AbstractSyntaxBlock {
    type Error = self::Error;

    /// Strict-canonical decode per RFC 9172 §3.6 + §4: ASB field encodings
    /// MUST conform to RFC 8949 Deterministically Encoded CBOR with **no
    /// indefinite-length carveout** (RFC 9171 §4.1's carveout does not
    /// apply here). Any non-shortest scalar, unexpected tag, or
    /// indefinite-length container is rejected with `NotCanonical`. The
    /// returned `shortest` flag is therefore always `true` on `Ok`.
    fn from_cbor(data: &[u8]) -> core::result::Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_sequence(data, |seq| {
            // Targets
            let targets = seq
                .parse_array(|a, s, tags| {
                    if !s || !tags.is_empty() || !a.is_definite() {
                        return Err(Error::NotCanonical);
                    }
                    let mut targets: SmallVec<[u64; 4]> = SmallVec::new();
                    // The third tuple element from try_parse on a
                    // FromCbor 3-tuple is the consumed `usize` length.
                    // `Untagged` refuses a tagged target id from its
                    // first byte; `!s` covers non-shortest encoding.
                    while let Some((Untagged(block), s, _)) =
                        a.try_parse::<(Untagged<u64>, bool, usize)>()?
                    {
                        if !s {
                            return Err(Error::NotCanonical);
                        }
                        // Check for duplicates
                        if targets.contains(&block) {
                            return Err(Error::DuplicateOpTarget);
                        }
                        targets.push(block);
                    }
                    Ok::<_, Error>(targets)
                })
                .map_field_err::<Error>("security targets")?;
            if targets.is_empty() {
                return Err(Error::NoTargets);
            }

            // Context
            let context = require_canonical(seq, "security context id", Error::NotCanonical)?;

            // BundleFlags
            let flags: u64 = require_canonical(seq, "security context flags", Error::NotCanonical)?;

            // Source
            let source = require_canonical(seq, "security source", Error::NotCanonical)?;
            if let eid::Eid::Null | eid::Eid::LocalNode { .. } = source {
                return Err(Error::InvalidSecuritySource);
            }

            // Context Parameters
            let parameters = if flags & 1 == 0 {
                HashMap::new()
            } else {
                parse_ranges(seq, 0)
                    .map_field_err::<Error>("security context parameters")?
                    .unwrap_or_default()
            };

            // Target Results
            let offset = seq.offset();
            let results = seq.parse_array(|a, s, tags| {
                if !s || !tags.is_empty() || !a.is_definite() {
                    return Err(Error::NotCanonical);
                }

                let mut results = HashMap::with_capacity(targets.len());
                let mut idx = 0;
                while let Some(target_results) =
                    parse_ranges(a, offset).map_field_err::<Error>("security results")?
                {
                    results.insert(
                        *targets.get(idx).ok_or(Error::MismatchedTargetResult)?,
                        target_results,
                    );
                    idx += 1;
                }
                Ok::<_, Error>(results)
            })?;

            if targets.len() != results.len() {
                return Err(Error::MismatchedTargetResult);
            }

            Ok((
                AbstractSyntaxBlock {
                    context,
                    source,
                    parameters,
                    results,
                },
                true,
            ))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

/// Decodes a definite-length untagged byte string from `data[range]`.
///
/// Per RFC 9172 §4 (deterministic CBOR, no §4.1 carveout), tagged or
/// indefinite-length byte strings are rejected with `NotCanonical`.
#[cfg(feature = "rfc9173")]
pub fn decode_box(range: Range<usize>, data: &[u8]) -> Result<Box<[u8]>> {
    let data = bounded_slice(data, range)?;
    hardy_cbor::decode::parse_value(data, |v, s, tags| match v {
        hardy_cbor::decode::Value::Bytes(r) if s && tags.is_empty() => Ok(data[r].into()),
        hardy_cbor::decode::Value::Bytes(_) | hardy_cbor::decode::Value::ByteStream(_) => {
            Err(Error::NotCanonical)
        }
        value => Err(hardy_cbor::decode::Error::IncorrectType(
            "Untagged definite-length byte string",
            value.item_type(tags),
        )
        .into()),
    })
    .map(|v| v.0)
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use hardy_cbor::encode::Encoder;

    use super::*;

    // An Abstract Syntax Block is a CBOR sequence (RFC 9172 §3.6), not an
    // array: emit each field into a bare encoder.
    fn emit_asb(targets: &[u64], source: &eid::Eid, result_sets: usize) -> Vec<u8> {
        let mut encoder = Encoder::new();
        encoder.emit_array(Some(targets.len()), |a| {
            for target in targets {
                a.emit(target);
            }
        });
        encoder.emit(&99u64); // unrecognised security context id
        encoder.emit(&0u64); // flags: no context parameters
        encoder.emit(source);
        encoder.emit_array(Some(result_sets), |a| {
            for _ in 0..result_sets {
                a.emit_array(Some(0), |_| {}); // empty per-target result set
            }
        });
        encoder.build()
    }

    fn ipn_source() -> eid::Eid {
        "ipn:1.0".parse().unwrap()
    }

    fn expect_error(data: &[u8]) -> Error {
        match hardy_cbor::decode::parse::<AbstractSyntaxBlock>(data) {
            Ok(_) => panic!("malformed ASB parsed successfully"),
            Err(e) => e,
        }
    }

    // A well-formed ASB with an unrecognised context parses cleanly.
    #[test]
    fn asb_unknown_context_accepted() {
        let data = emit_asb(&[1, 2], &ipn_source(), 2);
        let (asb, shortest, len) =
            hardy_cbor::decode::parse::<(AbstractSyntaxBlock, bool, usize)>(&data)
                .expect("should parse");
        assert!(matches!(asb.context, Context::Unrecognised(99)));
        assert_eq!(asb.source, ipn_source());
        assert_eq!(asb.results.len(), 2);
        assert!(shortest, "strict-canonical ASB decode returns shortest");
        assert_eq!(len, data.len());
    }

    // RFC 9172 §3.6: the security targets array must not be empty.
    #[test]
    fn asb_no_targets_rejected() {
        let data = emit_asb(&[], &ipn_source(), 0);
        assert!(matches!(expect_error(&data), Error::NoTargets));
    }

    // RFC 9172 §3.2 (uniqueness of security operations): the same target
    // must not appear twice in one security block. The error surfaces
    // wrapped as the targets field error.
    #[test]
    fn asb_duplicate_target_rejected() {
        let data = emit_asb(&[1, 1], &ipn_source(), 2);
        let Error::InvalidField {
            field: "security targets",
            source,
        } = expect_error(&data)
        else {
            panic!("duplicate target should fail on the security targets field");
        };
        assert!(matches!(source.as_ref(), Error::DuplicateOpTarget));
    }

    // RFC 9172 §3.6: the security results array must line up one-to-one with
    // the targets array, in both directions.
    #[test]
    fn asb_mismatched_target_result_rejected() {
        // Fewer result sets than targets.
        let data = emit_asb(&[1, 2], &ipn_source(), 1);
        assert!(matches!(expect_error(&data), Error::MismatchedTargetResult));

        // More result sets than targets.
        let data = emit_asb(&[1], &ipn_source(), 2);
        assert!(matches!(expect_error(&data), Error::MismatchedTargetResult));
    }

    // RFC 9172 §3.1: the security source must identify a node; the null and
    // local-node EIDs are not acceptable.
    #[test]
    fn asb_invalid_security_source_rejected() {
        let data = emit_asb(&[1], &eid::Eid::Null, 1);
        assert!(matches!(expect_error(&data), Error::InvalidSecuritySource));

        let data = emit_asb(&[1], &eid::Eid::LocalNode(1), 1);
        assert!(matches!(expect_error(&data), Error::InvalidSecuritySource));
    }
}
