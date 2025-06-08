use super::*;
use std::{collections::HashMap, ops::Range};

fn parse_ranges<const D: usize>(
    seq: &mut hardy_cbor::decode::Series<D>,
    shortest: &mut bool,
    mut offset: usize,
) -> Result<Option<HashMap<u64, Range<usize>>>, Error> {
    offset += seq.offset();
    seq.try_parse_array(|a, s, tags| {
        *shortest = *shortest && s && tags.is_empty() && a.is_definite();
        let mut outer_offset = a.offset();

        let mut map = HashMap::new();
        while let Some((id, r)) = a.try_parse_array(|a, s, tags| {
            *shortest = *shortest && s && tags.is_empty() && a.is_definite();

            let id = a
                .parse::<(u64, bool)>()
                .map(|(v, s)| {
                    *shortest = *shortest && s;
                    v
                })
                .map_field_err("id")?;

            let data_start = offset + outer_offset + a.offset();
            if a.skip_value(16).map_field_err("value")?.is_none() {
                return Err(hardy_cbor::decode::Error::NotEnoughData.into());
            };
            Ok::<_, Error>((id, data_start..offset + outer_offset + a.offset()))
        })? {
            map.insert(id, r);
            outer_offset = a.offset();
        }
        Ok(map)
    })
}

#[derive(Debug)]
pub struct UnknownOperation {
    parameters: Rc<HashMap<u64, Box<[u8]>>>,
    results: HashMap<u64, Box<[u8]>>,
}

impl UnknownOperation {
    pub fn parse(
        asb: AbstractSyntaxBlock,
        source_data: &[u8],
    ) -> Result<(eid::Eid, HashMap<u64, Self>), Error> {
        let parameters = Rc::from(asb.parameters.into_iter().fold(
            HashMap::new(),
            |mut map, (id, range)| {
                map.insert(id, source_data[range].into());
                map
            },
        ));

        // Unpack results
        let mut operations = HashMap::new();
        for (target, results) in asb.results {
            operations.insert(
                target,
                Self {
                    parameters: parameters.clone(),
                    results: results
                        .into_iter()
                        .fold(HashMap::new(), |mut map, (id, range)| {
                            map.insert(id, source_data[range].into());
                            map
                        }),
                },
            );
        }
        Ok((asb.source, operations))
    }

    pub fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid, id: u64) {
        encoder.emit(id);
        if self.parameters.is_empty() {
            encoder.emit(0);
            encoder.emit(source);
        } else {
            encoder.emit(1);
            encoder.emit(source);
            encoder.emit_array(Some(self.parameters.len()), |a| {
                for (id, result) in self.parameters.iter() {
                    a.emit_array(Some(2), |a| {
                        a.emit(*id);
                        a.emit_raw_slice(result);
                    });
                }
            });
        }
    }

    pub fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        array.emit_array(Some(self.results.len()), |a| {
            for (id, result) in &self.results {
                a.emit_array(Some(2), |a| {
                    a.emit(*id);
                    a.emit_raw_slice(result);
                });
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

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_sequence(data, |seq| {
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
                .map_field_err("security targets")?;
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
                .map_field_err("security context id")?;

            // Flags
            let flags: u64 = seq
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("security context flags")?;

            // Source
            let source = seq
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("security source")?;
            if let eid::Eid::Null | eid::Eid::LocalNode { .. } = source {
                return Err(Error::InvalidSecuritySource);
            }

            // Context Parameters
            let parameters = if flags & 1 == 0 {
                HashMap::new()
            } else {
                parse_ranges(seq, &mut shortest, 0)
                    .map_field_err("security context parameters")?
                    .unwrap_or_default()
            };

            // Target Results
            let offset = seq.offset();
            let results = seq.parse_array(|a, s, tags| {
                shortest = shortest && s && tags.is_empty() && a.is_definite();

                let mut results = HashMap::new();
                let mut idx = 0;
                while let Some(target_results) =
                    parse_ranges(a, &mut shortest, offset).map_field_err("security results")?
                {
                    let Some(target) = targets.get(idx) else {
                        return Err(Error::MismatchedTargetResult);
                    };

                    results.insert(*target, target_results);
                    idx += 1;
                }
                Ok(results)
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
                shortest,
            ))
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

pub fn decode_box(
    range: Range<usize>,
    data: &[u8],
) -> Result<(Box<[u8]>, bool), hardy_cbor::decode::Error> {
    hardy_cbor::decode::parse_value(&data[range.start..range.end], |v, s, tags| match v {
        hardy_cbor::decode::Value::Bytes(data) => Ok((data.into(), s && tags.is_empty())),
        hardy_cbor::decode::Value::ByteStream(data) => Ok((
            data.iter()
                .fold(Vec::new(), |mut data, d| {
                    data.extend(*d);
                    data
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
