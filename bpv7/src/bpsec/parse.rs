use super::*;
use std::{collections::HashMap, ops::Range};

#[derive(Debug)]
struct UnknownData(HashMap<u64, Box<[u8]>>);

impl UnknownData {
    fn from_cbor(
        ranges: HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), bpsec::Error> {
        let mut shortest = true;
        let mut new_ranges = HashMap::new();
        for (t, range) in ranges {
            let (value, s) =
                cbor::decode::parse::<(Box<[u8]>, bool)>(&data[range.start..range.end])?;
            shortest = shortest && s;
            new_ranges.insert(t, value);
        }
        Ok((Self(new_ranges), shortest))
    }
}

impl cbor::encode::ToCbor for &UnknownData {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
        encoder.emit_array(Some(self.0.len()), |a, _| {
            for (id, value) in &self.0 {
                a.emit_array(Some(2), |a, _| {
                    a.emit(*id);
                    a.emit(value.as_ref());
                });
            }
        })
    }
}

#[derive(Debug)]
pub struct UnknownOperation {
    parameters: Rc<UnknownData>,
    results: UnknownData,
}

impl UnknownOperation {
    pub fn parse(
        asb: parse::AbstractSyntaxBlock,
        data: &[u8],
        shortest: &mut bool,
    ) -> Result<(Eid, HashMap<u64, Self>), Error> {
        let parameters = Rc::from(
            UnknownData::from_cbor(asb.parameters, data)
                .map(|(p, s)| {
                    *shortest = *shortest && s;
                    p
                })
                .map_field_err("Unknown BPSec operation parameters")?,
        );

        // Unpack results
        let mut operations = HashMap::new();
        for (target, results) in asb.results {
            operations.insert(
                target,
                Self {
                    parameters: parameters.clone(),
                    results: UnknownData::from_cbor(results, data)
                        .map(|(v, s)| {
                            *shortest = *shortest && s;
                            v
                        })
                        .map_field_err("Unknown BPSec operation results")?,
                },
            );
        }
        Ok((asb.source, operations))
    }

    pub fn emit_context(
        &self,
        encoder: &mut cbor::encode::Encoder,
        source: &Eid,
        id: u64,
    ) -> usize {
        let mut len = encoder.emit(id);
        if self.parameters.0.is_empty() {
            len += encoder.emit(0);
            len + encoder.emit(source)
        } else {
            len += encoder.emit(1);
            len += encoder.emit(source);
            len + encoder.emit(self.parameters.as_ref())
        }
    }

    pub fn emit_result(&self, array: &mut cbor::encode::Array) {
        array.emit(&self.results);
    }
}

pub struct AbstractSyntaxBlock {
    pub context: Context,
    pub source: Eid,
    pub parameters: HashMap<u64, Range<usize>>,
    pub results: HashMap<u64, HashMap<u64, Range<usize>>>,
}

fn parse_ranges<const D: usize>(
    seq: &mut cbor::decode::Sequence<D>,
    shortest: &mut bool,
    mut offset: usize,
) -> Result<Option<HashMap<u64, Range<usize>>>, Error> {
    offset += seq.offset();
    seq.try_parse_array(|a, s, tags| {
        *shortest = *shortest && s && tags.is_empty() && a.is_definite();
        offset += a.offset();

        let mut map = HashMap::new();
        while let Some(((id, r), _)) = a.try_parse_array(|a, s, tags| {
            *shortest = *shortest && s && tags.is_empty() && a.is_definite();

            let id = a
                .parse::<(u64, bool)>()
                .map(|(v, s)| {
                    *shortest = *shortest && s;
                    v
                })
                .map_field_err("Id")?;

            let data_start = offset + a.offset();
            if let Some((_, data_len)) = a.skip_value(16).map_field_err("Value")? {
                Ok::<_, Error>((id, data_start..data_start + data_len))
            } else {
                Err(cbor::decode::Error::NotEnoughData.into())
            }
        })? {
            map.insert(id, r);
        }
        Ok(map)
    })
    .map(|o| o.map(|(v, _)| v))
}

impl cbor::decode::FromCbor for AbstractSyntaxBlock {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_structure(data, |seq| {
            let mut shortest = true;

            // Targets
            let targets = seq
                .parse_array(|a, s, tags| {
                    shortest = shortest && s && tags.is_empty() && a.is_definite();
                    let mut targets: Vec<u64> = Vec::new();
                    while let Some((block, s, _)) = a.try_parse()? {
                        shortest = shortest && s;

                        // Check for duplicates
                        if targets.contains(&block) {
                            return Err(Error::DuplicateOpTarget);
                        }
                        targets.push(block);
                    }
                    Ok::<_, Error>(targets)
                })
                .map_field_err("Security Targets field")?
                .0;
            if targets.is_empty() {
                return Err(Error::NoTargets);
            }

            // Context
            let context = seq
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Security Context Id field")?;

            // Flags
            let flags: u64 = seq
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Security Context Flags field")?;

            // Source
            let source = seq
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Security Source field")?;
            if let Eid::Null | Eid::LocalNode { .. } = source {
                return Err(Error::InvalidSecuritySource);
            }

            // Context Parameters
            let parameters = if flags & 1 == 0 {
                HashMap::new()
            } else {
                parse_ranges(seq, &mut shortest, 0)
                    .map_field_err("Security Context Parameters")?
                    .unwrap_or_default()
            };

            // Target Results
            let offset = seq.offset();
            let results = seq
                .parse_array(|a, s, tags| {
                    shortest = shortest && s && tags.is_empty() && a.is_definite();

                    let mut results = HashMap::new();
                    let mut idx = 0;
                    while let Some(target_results) =
                        parse_ranges(a, &mut shortest, offset).map_field_err("Security Results")?
                    {
                        let Some(target) = targets.get(idx) else {
                            return Err(Error::MismatchedTargetResult);
                        };

                        results.insert(*target, target_results);
                        idx += 1;
                    }
                    Ok(results)
                })
                .map_field_err("Security Targets field")?
                .0;

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
                shortest,
            ))
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
