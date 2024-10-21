use super::*;
use std::{collections::HashMap, ops::Range};
use thiserror::Error;

mod bcb_aes_gcm;
mod bib_hmac_sha2;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
pub enum SecurityContext {
    BIB_HMAC_SHA2,
    BCB_AES_GCM,
    Unrecognised(u64),
}

impl From<SecurityContext> for u64 {
    fn from(value: SecurityContext) -> Self {
        match value {
            SecurityContext::BIB_HMAC_SHA2 => 1,
            SecurityContext::BCB_AES_GCM => 2,
            SecurityContext::Unrecognised(v) => v,
        }
    }
}

impl From<u64> for SecurityContext {
    fn from(value: u64) -> Self {
        match value {
            1 => SecurityContext::BIB_HMAC_SHA2,
            2 => SecurityContext::BCB_AES_GCM,
            value => SecurityContext::Unrecognised(value),
        }
    }
}

pub struct SecurityBlock {
    pub context: SecurityContext,
    pub source: Eid,
    pub parameters: HashMap<u64, Range<usize>>,
    pub results: HashMap<u64, HashMap<u64, Range<usize>>>,
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Mismatch Target and Results arrays")]
    MismatchedTargetResult,

    #[error("Missing security target block")]
    MissingSecurityTarget,

    #[error("Invalid Null or LocalNode security source")]
    InvalidSecuritySource,

    #[error("BIBs must not target BIBs or BCBs")]
    InvalidBIBTarget,

    #[error("BCBs must not target BCBs, non-shared BIBs, or the primary block")]
    InvalidBCBTarget,

    #[error("Invalid security context parameter {0}")]
    InvalidContextParameter(u64),

    #[error("Invalid security context result id {0}")]
    InvalidContextResultId(u64),

    #[error("BCBs must have the 'Block must be replicated in every fragment' flag set if one of the targets is the payload block")]
    BCBMustReplicate,

    #[error("BCBs must not have the 'Block must be removed from bundle if it can't be processed' flag set.")]
    BCBDeleteFlag,

    #[error("BCBs must not target a BIB unless it shares a security target with that BIB")]
    BCBMustShareTarget,

    #[error("The same security service must not be applied to a security target more than once in a bundle")]
    DuplicateOpTarget,

    #[error("Invalid context id")]
    InvalidContext,

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

impl SecurityBlock {
    pub fn validate(
        &self,
        source: &Block,
        target: Option<&Block>,
        _data: &[u8],
    ) -> Result<(), Error> {
        if let Eid::Null | Eid::LocalNode { .. } = self.source {
            return Err(Error::InvalidSecuritySource);
        }

        let Some(target) = target else {
            return Err(Error::MissingSecurityTarget);
        };

        match (source.block_type, target.block_type) {
            (BlockType::BlockIntegrity, BlockType::BlockIntegrity)
            | (BlockType::BlockIntegrity, BlockType::BlockSecurity) => Err(Error::InvalidBIBTarget),
            (BlockType::BlockSecurity, BlockType::Primary)
            | (BlockType::BlockSecurity, BlockType::BlockSecurity) => Err(Error::InvalidBCBTarget),
            (BlockType::BlockSecurity, BlockType::Payload) if !source.flags.must_replicate => {
                Err(Error::BCBMustReplicate)
            }
            /*
             * We can't do the following as the block is ciphertext, but we ought to really!
             *
            (BlockType::BlockSecurity, BlockType::BlockIntegrity) => {
                let target: SecurityBlock = cbor::decode::parse(&target.block_data(data))?;
                let mut found = false;
                for t in target.results.keys() {
                    if self.results.contains_key(t) {
                        found = true;
                        break;
                    }
                }
                if !found {
                    Err(Error::BCBMustShareTarget)
                } else {
                    Ok(())
                }
            }*/
            (BlockType::BlockSecurity, _) if !source.flags.delete_block_on_failure => {
                Err(Error::BCBDeleteFlag)
            }
            (BlockType::BlockSecurity, _) => match self.context {
                SecurityContext::BIB_HMAC_SHA2 => Err(Error::InvalidContext),
                _ => Ok(()),
            },
            (BlockType::BlockIntegrity, _) => match self.context {
                SecurityContext::BCB_AES_GCM => Err(Error::InvalidContext),
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }
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
            let (context, s) = seq
                .parse::<(u64, bool)>()
                .map(|(v, s)| (v.into(), s))
                .map_field_err("Security Context Id field")?;
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
                parse_ranges(seq, &mut shortest, 0)
                    .map_field_err("Security Context Parameters")?
                    .unwrap_or_default()
            };

            match context {
                SecurityContext::BIB_HMAC_SHA2 => {
                    let (_, s) = bib_hmac_sha2::Parameters::from_cbor(&parameters, data)?;
                    shortest = shortest && s;
                }
                SecurityContext::BCB_AES_GCM => {
                    let (_, s) = bcb_aes_gcm::Parameters::from_cbor(&parameters, data)?;
                    shortest = shortest && s;
                }
                SecurityContext::Unrecognised(_) => {}
            }

            // Target Results
            let offset = seq.offset();
            let (results, _) = seq
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

                        match context {
                            SecurityContext::BIB_HMAC_SHA2 => {
                                let (_, s) =
                                    bib_hmac_sha2::Results::from_cbor(&target_results, data)?;
                                shortest = shortest && s;
                            }
                            SecurityContext::BCB_AES_GCM => {
                                let (_, s) =
                                    bcb_aes_gcm::Results::from_cbor(&target_results, data)?;
                                shortest = shortest && s;
                            }
                            SecurityContext::Unrecognised(_) => {}
                        }

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
                context,
                source,
                parameters,
                results,
            })
        })
        .map(|o| o.map(|(v, len)| (v, shortest, len)))
    }
}
