use super::*;
use crate::error::HasInvalidField;
use alloc::rc::Rc;
use core::ops::Range;
use smallvec::SmallVec;

fn require_canonical<T, const D: usize>(
    seq: &mut hardy_cbor::decode::Series<D>,
    field: &'static str,
) -> Result<T, Error>
where
    T: hardy_cbor::decode::FromCbor,
    T::Error: From<hardy_cbor::decode::Error> + Into<Box<dyn core::error::Error + Send + Sync>>,
{
    match seq.parse::<(T, bool)>() {
        Err(e) => Err(Error::invalid_field(field, e.into())),
        Ok((_, false)) => Err(Error::invalid_field(field, Error::NotCanonical.into())),
        Ok((t, true)) => Ok(t),
    }
}

/// Strict-canonical helper per RFC 9172 §4 — no §4.1 carveout for ASB
/// content, so every encoding violation (non-shortest, indefinite-
/// length, unexpected tags) is rejected with `NotCanonical`.
fn parse_ranges<const D: usize>(
    seq: &mut hardy_cbor::decode::Series<D>,
    mut offset: usize,
) -> Result<Option<HashMap<u64, Range<usize>>>, Error> {
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

                let id = require_canonical(a, "id")?;
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
    pub parameters: Rc<HashMap<u64, Box<[u8]>>>,
    pub results: HashMap<u64, Box<[u8]>>,
}

impl UnknownOperation {
    pub fn parse(
        asb: AbstractSyntaxBlock,
        source_data: &[u8],
    ) -> Result<(eid::Eid, HashMap<u64, Self>), Error> {
        let param_count = asb.parameters.len();
        let parameters = Rc::from(asb.parameters.into_iter().fold(
            HashMap::with_capacity(param_count),
            |mut map, (id, range)| {
                map.insert(id, source_data[range].into());
                map
            },
        ));

        // Unpack results
        let mut operations = HashMap::with_capacity(asb.results.len());
        for (target, results) in asb.results {
            let result_count = results.len();
            operations.insert(
                target,
                Self {
                    parameters: parameters.clone(),
                    results: results.into_iter().fold(
                        HashMap::with_capacity(result_count),
                        |mut map, (id, range)| {
                            map.insert(id, source_data[range].into());
                            map
                        },
                    ),
                },
            );
        }
        Ok((asb.source, operations))
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
    pub source: eid::Eid,
    pub parameters: HashMap<u64, Range<usize>>,
    pub results: HashMap<u64, HashMap<u64, Range<usize>>>,
}

impl hardy_cbor::decode::FromCbor for AbstractSyntaxBlock {
    type Error = self::Error;

    /// Strict-canonical decode per RFC 9172 §3.6 + §4: ASB field encodings
    /// MUST conform to RFC 8949 Deterministically Encoded CBOR with **no
    /// indefinite-length carveout** (RFC 9171 §4.1's carveout does not
    /// apply here). Any non-shortest scalar, unexpected tag, or
    /// indefinite-length container is rejected with `NotCanonical`. The
    /// returned `shortest` flag is therefore always `true` on `Ok`.
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_sequence(data, |seq| {
            // Targets
            let targets = seq
                .parse_array(|a, s, tags| {
                    if !s || !tags.is_empty() || !a.is_definite() {
                        return Err(Error::NotCanonical);
                    }
                    let mut targets: SmallVec<[u64; 4]> = SmallVec::new();
                    // The third tuple element from try_parse on a
                    // FromCbor 3-tuple is the consumed `usize` length;
                    // u64's FromCbor folds tag-emptiness into the
                    // `shortest` flag, so checking `!s` here covers
                    // both non-shortest and unexpected tags.
                    while let Some((block, s, _)) = a.try_parse::<(u64, bool, usize)>()? {
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
            let context = require_canonical(seq, "security context id")?;

            // Flags
            let flags: u64 = require_canonical(seq, "security context flags")?;

            // Source
            let source = require_canonical(seq, "security source")?;
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

#[cfg(feature = "rfc9173")]
pub fn decode_box(
    range: Range<usize>,
    data: &[u8],
) -> Result<(Box<[u8]>, bool), hardy_cbor::decode::Error> {
    let data = &data[range.start..range.end];
    hardy_cbor::decode::parse_value(data, |v, s, tags| match v {
        hardy_cbor::decode::Value::Bytes(r) => Ok((data[r].into(), s && tags.is_empty())),
        hardy_cbor::decode::Value::ByteStream(ranges) => Ok((
            ranges
                .into_iter()
                .fold(Vec::new(), |mut acc, r| {
                    acc.extend_from_slice(&data[r]);
                    acc
                })
                .into(),
            false,
        )),
        value => Err(hardy_cbor::decode::Error::IncorrectType(
            "Untagged definite-length byte string".to_string(),
            value.type_name(!tags.is_empty()),
        )),
    })
    .map(|v| v.0)
}
