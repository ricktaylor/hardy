use super::*;
use std::{collections::HashMap, ops::Range};
use thiserror::Error;

pub struct SecurityBlock {
    context_id: u64,
    source: Eid,
    parameters: HashMap<u64, Range<usize>>,
    results: HashMap<u64, HashMap<u64, Range<usize>>>,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Mismatch Target and Results arrays")]
    MismatchedTargetResult,

    #[error("Failed to parse {field}: {source}")]
    InvalidField {
        field: &'static str,
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    InvalidCBOR(#[from] cbor::decode::Error),
}

trait CaptureFieldErr<T> {
    fn map_field_err(self, field: &'static str) -> Result<T, Error>;
}

impl<T, E: Into<Box<dyn std::error::Error + Send + Sync>>> CaptureFieldErr<T>
    for std::result::Result<T, E>
{
    fn map_field_err(self, field: &'static str) -> Result<T, Error> {
        self.map_err(|e| Error::InvalidField {
            field,
            source: e.into(),
        })
    }
}

fn parse_ranges<const D: usize>(
    seq: &mut cbor::decode::Sequence<D>,
    shortest: &mut bool,
) -> Result<Option<HashMap<u64, Range<usize>>>, Error> {
    let mut offset = seq.offset();
    seq.try_parse_array(|a, s, tags| {
        *shortest = *shortest && s && tags.is_empty() && a.is_definite();
        offset += a.offset();

        let mut map = HashMap::new();
        while let Some(((id, r), _)) = a.try_parse_array(|a, s, tags| {
            *shortest = *shortest && s && tags.is_empty() && a.is_definite();

            let (id, s) = a.parse::<(u64, bool)>().map_field_err("Id")?;
            *shortest = *shortest && s;

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

impl cbor::decode::FromCbor for SecurityBlock {
    type Error = self::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        let mut shortest = true;
        cbor::decode::try_parse_structure(data, |seq| {
            // Targets
            let (targets, _) = seq
                .parse_array(|a, s, tags| {
                    shortest = s && tags.is_empty() && a.is_definite();
                    let mut targets: Vec<u64> = Vec::new();
                    while let Some((block, s, _)) = a.try_parse()? {
                        shortest = shortest && s;
                        targets.push(block);
                    }
                    Ok::<_, Error>(targets)
                })
                .map_field_err("Security Targets field")?;

            // Id
            let (context_id, s) = seq.parse().map_field_err("Security Context Id field")?;
            shortest = shortest && s;

            // Flags
            let (flags, s) = seq
                .parse::<(u64, bool)>()
                .map_field_err("Security Context Flags field")?;
            shortest = shortest && s;

            // Source
            let (source, s) = seq.parse().map_field_err("Security Source field")?;
            shortest = shortest && s;

            // Context Parameters
            let parameters = if flags & 1 == 0 {
                HashMap::new()
            } else {
                parse_ranges(seq, &mut shortest)
                    .map_field_err("Security Context Parameters")?
                    .unwrap_or_default()
            };

            // Target Results
            let (results, _) = seq
                .parse_array(|a, s, tags| {
                    shortest = shortest && s && tags.is_empty() && a.is_definite();

                    let mut results = HashMap::new();
                    let mut idx = 0;
                    while let Some(target_results) =
                        parse_ranges(a, &mut shortest).map_field_err("Security Results")?
                    {
                        let Some(target) = targets.get(idx) else {
                            return Err(Error::MismatchedTargetResult);
                        };

                        results.insert(*target, target_results);
                        idx += 1;
                    }

                    if targets.len() > idx {
                        Err(Error::MismatchedTargetResult)
                    } else {
                        Ok(results)
                    }
                })
                .map_field_err("Security Targets field")?;

            Ok(Self {
                context_id,
                source,
                parameters,
                results,
            })
        })
        .map(|o| o.map(|(v, len)| (v, shortest, len)))
    }
}
