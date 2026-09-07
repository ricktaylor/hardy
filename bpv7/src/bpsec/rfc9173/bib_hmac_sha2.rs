use alloc::{boxed::Box, string::ToString, sync::Arc};
use core::ops::Range;

use hardy_cbor::{
    decode::FromCbor,
    encode::{Array, Encoder, ToCbor, emit},
};
use hmac::{KeyInit, Mac};

use super::{ScopeFlags, canonical_primary, key_wrap::KeyWrap, mac_tag::MacTag, rand_bytes};
use crate::{
    HashMap, block,
    bpsec::{Context, Error, bib, key, parse},
    eid,
};

/// The RFC 9173 §3.3.1 variant parameter. A foreign wire value is a
/// legitimate RFC 9172 pass-through state, carried as `Unrecognised`;
/// `as_variant` never produces it, so it cannot reach the sign dispatch.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ShaVariant {
    HMAC_256_256,
    #[default]
    HMAC_384_384,
    HMAC_512_512,
    Unrecognised(u64),
}

impl ToCbor for ShaVariant {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        match self {
            Self::HMAC_256_256 => encoder.emit(&5),
            Self::HMAC_384_384 => encoder.emit(&6),
            Self::HMAC_512_512 => encoder.emit(&7),
            Self::Unrecognised(v) => encoder.emit(v),
        }
    }
}

impl FromCbor for ShaVariant {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (value, len) = crate::error::parse_canonical::<u64, _>(data, Error::NotCanonical)?;
        Ok((
            match value {
                5 => Self::HMAC_256_256,
                6 => Self::HMAC_384_384,
                7 => Self::HMAC_512_512,
                v => Self::Unrecognised(v),
            },
            true,
            len,
        ))
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parameters {
    pub variant: ShaVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: ScopeFlags,
}

impl Parameters {
    fn from_cbor(parameters: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<Self, Error> {
        let mut result = Self::default();
        for (id, range) in parameters {
            match id {
                1 => {
                    result.variant = hardy_cbor::decode::parse(parse::bounded_slice(data, range)?)?
                }
                2 => result.key = Some(parse::decode_box(range, data)?),
                3 => result.flags = hardy_cbor::decode::parse(parse::bounded_slice(data, range)?)?,
                _ => return Err(Error::InvalidContextParameter(id)),
            }
        }
        Ok(result)
    }
}

impl ToCbor for Parameters {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        let mut mask: u32 = 0;
        if self.variant != ShaVariant::default() {
            mask |= 1 << 1;
        }
        if self.key.is_some() {
            mask |= 1 << 2;
        }
        if self.flags != ScopeFlags::default() {
            mask |= 1 << 3;
        }
        encoder.emit_array(Some(mask.count_ones() as usize), |a| {
            for b in 1..=3 {
                if mask & (1 << b) != 0 {
                    match b {
                        1 => a.emit(&(b, &self.variant)),
                        2 => a.emit(&(b, &hardy_cbor::encode::Bytes(self.key.as_ref().unwrap()))),
                        3 => a.emit(&(b, &self.flags)),
                        _ => unreachable!("loop range is 1..=3"),
                    }
                }
            }
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Results(pub MacTag);

impl Results {
    fn from_cbor(results: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<Self, Error> {
        let mut r = None;
        for (id, range) in results {
            match id {
                1 => r = Some(parse::decode_box(range, data)?),
                _ => return Err(Error::InvalidContextResult(id)),
            }
        }

        Ok(Self(MacTag::from_bytes(
            r.ok_or(Error::InvalidContextResult(1))?,
        )))
    }
}

impl ToCbor for Results {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&[&(1, &self.0)]);
    }
}

fn build_hmac<D>(
    flags: &ScopeFlags,
    key: &[u8],
    args: &bib::OperationArgs,
) -> Result<hmac::Hmac<D>, Error>
where
    D: hmac::EagerHash,
{
    let mut mac =
        hmac::Hmac::<D>::new_from_slice(key).map_err(|e| Error::Algorithm(e.to_string()))?;

    // Build IPT
    mac.update(
        &emit(&ScopeFlags {
            include_primary_block: flags.include_primary_block,
            include_target_header: flags.include_target_header,
            include_security_header: flags.include_security_header,
            ..Default::default()
        })
        .0,
    );

    let (target_block, payload) = args
        .blocks
        .block(args.target)
        .ok_or(Error::MissingSecurityTarget)?;
    let payload = payload.ok_or(Error::MissingSecurityTarget)?;

    if !matches!(target_block.block_type, block::Type::Primary) {
        if flags.include_primary_block {
            let raw = args
                .blocks
                .block(0)
                .and_then(|v| v.1)
                .expect("Missing primary block!");
            let raw = raw.as_ref();
            // RFC 9172 §4: IPPT requires the canonical (deterministic) form.
            mac.update(&canonical_primary(raw)?);
        }

        if flags.include_target_header {
            let mut encoder = Encoder::new();
            encoder.emit(&target_block.block_type);
            encoder.emit(&args.target);
            encoder.emit(&target_block.flags);
            mac.update(&encoder.build());
        }
    }

    if flags.include_security_header {
        let source_block = args
            .blocks
            .block(args.source)
            .ok_or(Error::MissingSecurityTarget)?
            .0;
        let mut encoder = Encoder::new();
        encoder.emit(&source_block.block_type);
        encoder.emit(&args.source);
        encoder.emit(&source_block.flags);
        mac.update(&encoder.build());
    }

    // Step 5: CBOR byte-string encoding of the security target's canonical form
    // (confirmed by RFC 9173 Appendix A.3 test vector: primary block is also
    // byte-string-wrapped). For primary block targets, canonicalize before wrapping.
    if matches!(target_block.block_type, block::Type::Primary) {
        // RFC 9172 §4: IPPT requires the canonical (deterministic) form.
        let bytes = canonical_primary(payload.as_ref())?;
        mac.update(&emit(&hardy_cbor::encode::BytesHeader(bytes.len() as u64)).0);
        mac.update(&bytes);
    } else {
        // Reduce copying by emitting the byte-string header separately.
        mac.update(&emit(&hardy_cbor::encode::BytesHeader(payload.len() as u64)).0);
        mac.update(payload.as_ref());
    }

    Ok(mac)
}

fn as_key_wrap(alg: Option<key::KeyAlgorithm>) -> Option<KeyWrap> {
    match alg {
        Some(key::KeyAlgorithm::A128KW)
        | Some(key::KeyAlgorithm::HS256_A128KW)
        | Some(key::KeyAlgorithm::HS384_A128KW)
        | Some(key::KeyAlgorithm::HS512_A128KW) => Some(KeyWrap::Aes128),

        Some(key::KeyAlgorithm::A192KW)
        | Some(key::KeyAlgorithm::HS256_A192KW)
        | Some(key::KeyAlgorithm::HS384_A192KW)
        | Some(key::KeyAlgorithm::HS512_A192KW) => Some(KeyWrap::Aes192),

        Some(key::KeyAlgorithm::A256KW)
        | Some(key::KeyAlgorithm::HS256_A256KW)
        | Some(key::KeyAlgorithm::HS384_A256KW)
        | Some(key::KeyAlgorithm::HS512_A256KW) => Some(KeyWrap::Aes256),

        _ => None,
    }
}

fn as_variant(alg: Option<key::KeyAlgorithm>) -> Option<ShaVariant> {
    match alg {
        Some(key::KeyAlgorithm::HS256)
        | Some(key::KeyAlgorithm::HS256_A128KW)
        | Some(key::KeyAlgorithm::HS256_A192KW)
        | Some(key::KeyAlgorithm::HS256_A256KW) => Some(ShaVariant::HMAC_256_256),

        None
        | Some(key::KeyAlgorithm::HS384)
        | Some(key::KeyAlgorithm::HS384_A128KW)
        | Some(key::KeyAlgorithm::HS384_A192KW)
        | Some(key::KeyAlgorithm::HS384_A256KW)
        | Some(key::KeyAlgorithm::A128KW)
        | Some(key::KeyAlgorithm::A192KW)
        | Some(key::KeyAlgorithm::A256KW) => Some(ShaVariant::HMAC_384_384),

        Some(key::KeyAlgorithm::HS512)
        | Some(key::KeyAlgorithm::HS512_A128KW)
        | Some(key::KeyAlgorithm::HS512_A192KW)
        | Some(key::KeyAlgorithm::HS512_A256KW) => Some(ShaVariant::HMAC_512_512),

        _ => None,
    }
}

#[derive(Debug)]
pub struct Operation {
    pub parameters: Arc<Parameters>,
    pub results: Results,
}

impl Operation {
    pub fn is_unsupported(&self) -> bool {
        matches!(self.parameters.variant, ShaVariant::Unrecognised(_))
    }

    pub fn sign(
        jwk: &key::Key,
        scope_flags: ScopeFlags,
        args: bib::OperationArgs,
    ) -> Result<Self, Error> {
        if let Some(ops) = &jwk.operations
            && !ops.contains(&key::Operation::Sign)
        {
            return Err(Error::InvalidKey(key::Operation::Sign, jwk.clone()));
        }

        let variant = as_variant(jwk.key_algorithm)
            .ok_or_else(|| Error::InvalidKey(key::Operation::Sign, jwk.clone()))?;
        let key_wrap = as_key_wrap(jwk.key_algorithm);

        let cek = if let Some(key_wrap) = &key_wrap {
            if let Some(ops) = &jwk.operations
                && !ops.contains(&key::Operation::WrapKey)
            {
                return Err(Error::InvalidKey(key::Operation::WrapKey, jwk.clone()));
            }
            Some(zeroize::Zeroizing::from(match key_wrap {
                KeyWrap::Aes128 => rand_bytes::<32>()?,
                KeyWrap::Aes192 => rand_bytes::<48>()?,
                KeyWrap::Aes256 => rand_bytes::<64>()?,
            }))
        } else {
            None
        };

        let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
            return Err(Error::InvalidKey(key::Operation::Sign, jwk.clone()));
        };

        let active_cek = cek.as_ref().map_or(
            kek.expose_secret(),
            |cek: &zeroize::Zeroizing<Box<[u8]>>| cek.as_ref(),
        );

        let results = Results(match variant {
            ShaVariant::HMAC_256_256 => {
                MacTag::from_mac(build_hmac::<sha2::Sha256>(&scope_flags, active_cek, &args)?)
            }
            ShaVariant::HMAC_384_384 => {
                MacTag::from_mac(build_hmac::<sha2::Sha384>(&scope_flags, active_cek, &args)?)
            }
            ShaVariant::HMAC_512_512 => {
                MacTag::from_mac(build_hmac::<sha2::Sha512>(&scope_flags, active_cek, &args)?)
            }
            // Dead in practice: `as_variant` never yields `Unrecognised`.
            ShaVariant::Unrecognised(_) => {
                return Err(Error::InvalidKey(key::Operation::Sign, jwk.clone()));
            }
        });

        let key = if let (Some(cek), Some(key_wrap)) = (cek, key_wrap) {
            Some(
                key_wrap
                    .wrap_key(kek.expose_secret(), &cek)
                    .map_err(Error::Algorithm)?
                    .into(),
            )
        } else {
            None
        };

        Ok(Self {
            parameters: Arc::new(Parameters {
                variant,
                key,
                flags: scope_flags,
            }),
            results,
        })
    }

    pub fn verify<K>(&self, key_source: &K, args: bib::OperationArgs) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
        if let Some(wrapped_cek) = &self.parameters.key {
            // Key wrapping mode - need a KEK to unwrap
            let jwk = key_source
                .key(
                    args.bpsec_source,
                    &[key::Operation::UnwrapKey, key::Operation::Verify],
                )
                .ok_or(Error::NoKey)?;

            if Some(self.parameters.variant) != as_variant(jwk.key_algorithm) {
                return Err(Error::IntegrityCheckFailed);
            }

            let key::Type::OctetSequence { key } = &jwk.key_type else {
                return Err(Error::IntegrityCheckFailed);
            };

            let cek = as_key_wrap(jwk.key_algorithm)
                .ok_or(Error::IntegrityCheckFailed)?
                .unwrap_key(key.expose_secret(), wrapped_cek)
                .map_err(|_| Error::IntegrityCheckFailed)?;

            if self.verify_inner(&cek, &args)? {
                Ok(())
            } else {
                Err(Error::IntegrityCheckFailed)
            }
        } else {
            // Direct mode - need a verification key
            let jwk = key_source
                .key(args.bpsec_source, &[key::Operation::Verify])
                .ok_or(Error::NoKey)?;

            if Some(self.parameters.variant) != as_variant(jwk.key_algorithm) {
                return Err(Error::IntegrityCheckFailed);
            }

            let key::Type::OctetSequence { key } = &jwk.key_type else {
                return Err(Error::IntegrityCheckFailed);
            };

            if self.verify_inner(key.expose_secret(), &args)? {
                Ok(())
            } else {
                Err(Error::IntegrityCheckFailed)
            }
        }
    }

    fn verify_inner(&self, cek: &[u8], args: &bib::OperationArgs) -> Result<bool, Error> {
        Ok(match self.parameters.variant {
            ShaVariant::HMAC_256_256 => self.results.0.verify(build_hmac::<sha2::Sha256>(
                &self.parameters.flags,
                cek,
                args,
            )?),
            ShaVariant::HMAC_384_384 => self.results.0.verify(build_hmac::<sha2::Sha384>(
                &self.parameters.flags,
                cek,
                args,
            )?),
            ShaVariant::HMAC_512_512 => self.results.0.verify(build_hmac::<sha2::Sha512>(
                &self.parameters.flags,
                cek,
                args,
            )?),
            // A foreign wire variant is a legitimate RFC 9172 pass-through
            // state; this node just cannot verify it.
            ShaVariant::Unrecognised(_) => return Err(Error::UnsupportedOperation),
        })
    }

    pub fn emit_context(&self, encoder: &mut Encoder, source: &eid::Eid) {
        encoder.emit(&Context::BIB_HMAC_SHA2);
        if self.parameters.as_ref() == &Parameters::default() {
            encoder.emit(&0);
            encoder.emit(source);
        } else {
            encoder.emit(&1);
            encoder.emit(source);
            encoder.emit(self.parameters.as_ref());
        }
    }

    pub fn emit_result(&self, array: &mut Array) {
        array.emit(&self.results);
    }
}

pub fn parse(
    asb: parse::AbstractSyntaxBlock,
    data: &[u8],
) -> Result<(eid::Eid, HashMap<u64, bib::Operation>), Error> {
    asb.into_operations(
        data,
        "RFC9173 HMAC-SHA2 parameters",
        "RFC9173 HMAC-SHA2 results",
        Parameters::from_cbor,
        Results::from_cbor,
        |parameters, results| {
            bib::Operation::HMAC_SHA2(Operation {
                parameters,
                results,
            })
        },
    )
}
