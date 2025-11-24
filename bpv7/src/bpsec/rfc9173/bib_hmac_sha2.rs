use super::*;
use aes_gcm::aes::cipher::BlockSizeUser;
use alloc::rc::Rc;
use hmac::{
    Mac,
    digest::{
        HashMarker, block_buffer,
        consts::U256,
        core_api::{BufferKindUser, CoreProxy, FixedOutputCore, UpdateCore},
        typenum,
    },
};
use rand::{TryRngCore, rngs::OsRng};

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

impl hardy_cbor::encode::ToCbor for ShaVariant {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        match self {
            Self::HMAC_256_256 => encoder.emit(&5),
            Self::HMAC_384_384 => encoder.emit(&6),
            Self::HMAC_512_512 => encoder.emit(&7),
            Self::Unrecognised(v) => encoder.emit(v),
        }
    }
}

impl hardy_cbor::decode::FromCbor for ShaVariant {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map(|(value, shortest, len)| {
            (
                match value {
                    5 => Self::HMAC_256_256,
                    6 => Self::HMAC_384_384,
                    7 => Self::HMAC_512_512,
                    v => Self::Unrecognised(v),
                },
                shortest,
                len,
            )
        })
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Parameters {
    pub variant: ShaVariant,
    pub key: Option<Box<[u8]>>,
    pub flags: ScopeFlags,
}

impl Parameters {
    fn from_cbor(
        parameters: HashMap<u64, Range<usize>>,
        data: &[u8],
    ) -> Result<(Self, bool), Error> {
        let mut shortest = true;
        let mut result = Self::default();
        for (id, range) in parameters {
            match id {
                1 => {
                    result.variant = hardy_cbor::decode::parse(&data[range]).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?;
                }
                2 => {
                    result.key = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                3 => {
                    result.flags = hardy_cbor::decode::parse(&data[range]).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?;
                }
                _ => return Err(Error::InvalidContextParameter(id)),
            }
        }
        Ok((result, shortest))
    }
}

impl hardy_cbor::encode::ToCbor for Parameters {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
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
                        _ => unreachable!(),
                    }
                }
            }
        })
    }
}

#[derive(Debug)]
pub struct Results(pub Box<[u8]>);

impl Results {
    fn from_cbor(results: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<(Self, bool), Error> {
        let mut shortest = true;
        let mut r = None;
        for (id, range) in results {
            match id {
                1 => {
                    r = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                _ => return Err(Error::InvalidContextResult(id)),
            }
        }

        Ok((Self(r.ok_or(Error::InvalidContextResult(1))?), shortest))
    }
}

impl hardy_cbor::encode::ToCbor for Results {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        encoder.emit(&[&(1, &hardy_cbor::encode::Bytes(&self.0))]);
    }
}

fn calculate_hmac<A>(
    flags: &ScopeFlags,
    key: &[u8],
    args: &bib::OperationArgs,
) -> Result<hmac::digest::Output<hmac::Hmac<A>>, Error>
where
    A: CoreProxy,
    <A as CoreProxy>::Core: HashMarker
        + UpdateCore
        + FixedOutputCore
        + BufferKindUser<BufferKind = block_buffer::Eager>
        + Default
        + Clone,
    <<A as CoreProxy>::Core as BlockSizeUser>::BlockSize: typenum::IsLess<U256>,
    typenum::Le<<<A as CoreProxy>::Core as BlockSizeUser>::BlockSize, U256>: typenum::NonZero,
{
    let mut mac =
        hmac::Hmac::<A>::new_from_slice(key).map_err(|e| Error::Algorithm(e.to_string()))?;

    // Build IPT
    mac.update(
        &hardy_cbor::encode::emit(&ScopeFlags {
            include_primary_block: flags.include_primary_block,
            include_target_header: flags.include_target_header,
            include_security_header: flags.include_security_header,
            ..Default::default()
        })
        .0,
    );

    if !matches!(args.target_block.block_type, block::Type::Primary) {
        if flags.include_primary_block {
            mac.update(args.blocks.primary_block().as_ref());
        }

        if flags.include_target_header {
            let mut encoder = hardy_cbor::encode::Encoder::new();
            encoder.emit(&args.target_block.block_type);
            encoder.emit(&args.target);
            encoder.emit(&args.target_block.flags);
            mac.update(&encoder.build());
        }
    }

    if flags.include_security_header {
        let mut encoder = hardy_cbor::encode::Encoder::new();
        encoder.emit(&args.source_block.block_type);
        encoder.emit(&args.source);
        encoder.emit(&args.source_block.flags);
        mac.update(&encoder.build());
    }

    let payload = args
        .blocks
        .block_payload(args.target, args.target_block)
        .ok_or(Error::MissingSecurityTarget)?;

    // Reduce copying here
    mac.update(&hardy_cbor::encode::emit(&hardy_cbor::encode::BytesHeader(&payload)).0);
    mac.update(payload.as_ref());

    Ok(mac.finalize().into_bytes())
}

fn rand_key(mut cek: Box<[u8]>) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error> {
    OsRng
        .try_fill_bytes(&mut cek)
        .map_err(|e| Error::Algorithm(e.to_string()))?;
    Ok(zeroize::Zeroizing::from(cek))
}

enum KeyWrap {
    Aes128,
    Aes192,
    Aes256,
}

fn as_key_wrap(alg: &Option<key::KeyAlgorithm>) -> Option<KeyWrap> {
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

fn as_variant(alg: &Option<key::KeyAlgorithm>) -> Option<ShaVariant> {
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
    pub parameters: Rc<Parameters>,
    pub results: Results,
}

impl Operation {
    pub fn is_unsupported(&self) -> bool {
        matches!(self.parameters.variant, ShaVariant::Unrecognised(_))
    }

    pub fn sign(
        jwk: &Key,
        scope_flags: ScopeFlags,
        args: bib::OperationArgs,
    ) -> Result<Self, Error> {
        if !matches!(args.target_block.crc_type, crc::CrcType::None) {
            return Err(Error::CrcPresent);
        }

        if let Some(ops) = &jwk.operations
            && !ops.contains(&key::Operation::Sign)
        {
            return Err(Error::InvalidKey(key::Operation::Sign, jwk.clone()));
        }

        let variant = as_variant(&jwk.key_algorithm).ok_or(Error::NoValidKey)?;
        let key_wrap = as_key_wrap(&jwk.key_algorithm);

        let cek = if let Some(key_wrap) = &key_wrap {
            match key_wrap {
                KeyWrap::Aes128 => {
                    if let Some(ops) = &jwk.operations
                        && !ops.contains(&key::Operation::WrapKey)
                    {
                        return Err(Error::InvalidKey(key::Operation::WrapKey, jwk.clone()));
                    }
                    Some(rand_key(Box::from([0u8; 32]))?)
                }
                KeyWrap::Aes192 => {
                    if let Some(ops) = &jwk.operations
                        && !ops.contains(&key::Operation::WrapKey)
                    {
                        return Err(Error::InvalidKey(key::Operation::WrapKey, jwk.clone()));
                    }
                    Some(rand_key(Box::from([0u8; 48]))?)
                }
                KeyWrap::Aes256 => {
                    if let Some(ops) = &jwk.operations
                        && !ops.contains(&key::Operation::WrapKey)
                    {
                        return Err(Error::InvalidKey(key::Operation::WrapKey, jwk.clone()));
                    }
                    Some(rand_key(Box::from([0u8; 64]))?)
                }
            }
        } else {
            None
        };

        let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
            return Err(Error::NoValidKey);
        };

        let active_cek = cek
            .as_ref()
            .map_or(kek.as_ref(), |cek: &zeroize::Zeroizing<Box<[u8]>>| {
                cek.as_ref()
            });

        let results = Results(match variant {
            ShaVariant::HMAC_256_256 => {
                Box::from(calculate_hmac::<sha2::Sha256>(&scope_flags, active_cek, &args)?.as_ref())
            }
            ShaVariant::HMAC_384_384 => {
                Box::from(calculate_hmac::<sha2::Sha384>(&scope_flags, active_cek, &args)?.as_ref())
            }
            ShaVariant::HMAC_512_512 => {
                Box::from(calculate_hmac::<sha2::Sha512>(&scope_flags, active_cek, &args)?.as_ref())
            }
            ShaVariant::Unrecognised(_) => unreachable!(),
        });

        let key = if let (Some(cek), Some(key_wrap)) = (cek, key_wrap) {
            let key = match key_wrap {
                KeyWrap::Aes128 => aes_kw::KekAes128::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                KeyWrap::Aes192 => aes_kw::KekAes192::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
                KeyWrap::Aes256 => aes_kw::KekAes256::try_from(kek.as_ref())
                    .and_then(|kek| kek.wrap_vec(&cek))
                    .map_err(|e| Error::Algorithm(e.to_string())),
            }?;
            Some(key.into())
        } else {
            None
        };

        Ok(Self {
            parameters: Rc::new(Parameters {
                variant,
                key,
                flags: scope_flags,
            }),
            results,
        })
    }

    pub fn verify(
        &self,
        key_f: &impl key::KeyStore,
        args: bib::OperationArgs,
    ) -> Result<(), Error> {
        if !matches!(args.target_block.crc_type, crc::CrcType::None) {
            return Err(Error::CrcPresent);
        }

        let mut tried_to_verify = false;

        if let Some(cek) = &self.parameters.key {
            for jwk in key_f.decrypt_keys(
                args.bpsec_source,
                &[key::Operation::UnwrapKey, key::Operation::Verify],
            ) {
                if Some(self.parameters.variant) == as_variant(&jwk.key_algorithm)
                    && let key::Type::OctetSequence { key } = &jwk.key_type
                    && let Some(cek) = (match as_key_wrap(&jwk.key_algorithm) {
                        Some(KeyWrap::Aes128) => aes_kw::KekAes128::try_from(key.as_ref())
                            .and_then(|key| key.unwrap_vec(cek))
                            .ok(),
                        Some(KeyWrap::Aes192) => aes_kw::KekAes192::try_from(key.as_ref())
                            .and_then(|key| key.unwrap_vec(cek))
                            .ok(),
                        Some(KeyWrap::Aes256) => aes_kw::KekAes256::try_from(key.as_ref())
                            .and_then(|key| key.unwrap_vec(cek))
                            .ok(),
                        _ => None,
                    })
                    .map(|v| zeroize::Zeroizing::from(Box::from(v)))
                    && self.verify_inner(&mut tried_to_verify, &cek, &args)? == Some(true)
                {
                    return Ok(());
                }
            }
        } else {
            for jwk in key_f.decrypt_keys(args.bpsec_source, &[key::Operation::Verify]) {
                if Some(self.parameters.variant) == as_variant(&jwk.key_algorithm)
                    && let key::Type::OctetSequence { key } = &jwk.key_type
                    && self.verify_inner(&mut tried_to_verify, key, &args)? == Some(true)
                {
                    return Ok(());
                }
            }
        }

        if tried_to_verify {
            Err(Error::IntegrityCheckFailed)
        } else {
            Err(Error::NoValidKey)
        }
    }

    fn verify_inner(
        &self,
        tried_to_verify: &mut bool,
        cek: &[u8],
        args: &bib::OperationArgs,
    ) -> Result<Option<bool>, Error> {
        match self.parameters.variant {
            ShaVariant::HMAC_256_256 => {
                if let Ok(mac) = calculate_hmac::<sha2::Sha256>(&self.parameters.flags, cek, args) {
                    *tried_to_verify = true;
                    return Ok(Some(*mac == *self.results.0));
                }
            }
            ShaVariant::HMAC_384_384 => {
                if let Ok(mac) = calculate_hmac::<sha2::Sha384>(&self.parameters.flags, cek, args) {
                    *tried_to_verify = true;
                    return Ok(Some(*mac == *self.results.0));
                }
            }
            ShaVariant::HMAC_512_512 => {
                if let Ok(mac) = calculate_hmac::<sha2::Sha512>(&self.parameters.flags, cek, args) {
                    *tried_to_verify = true;
                    return Ok(Some(*mac == *self.results.0));
                }
            }
            ShaVariant::Unrecognised(_) => {
                return Err(Error::UnsupportedOperation);
            }
        }
        Ok(None)
    }

    pub fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
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

    pub fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        array.emit(&self.results);
    }
}

pub fn parse(
    asb: parse::AbstractSyntaxBlock,
    data: &[u8],
) -> Result<(eid::Eid, HashMap<u64, bib::Operation>, bool), Error> {
    let mut shortest = false;
    let parameters = Rc::from(
        Parameters::from_cbor(asb.parameters, data)
            .map(|(p, s)| {
                shortest = s;
                p
            })
            .map_field_err("RFC9173 HMAC-SHA2 parameters")?,
    );

    // Unpack results
    let mut operations = HashMap::new();
    for (target, results) in asb.results {
        operations.insert(
            target,
            bib::Operation::HMAC_SHA2(Operation {
                parameters: parameters.clone(),
                results: Results::from_cbor(results, data)
                    .map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })
                    .map_field_err("RFC9173 HMAC-SHA2 results")?,
            }),
        );
    }
    Ok((asb.source, operations, shortest))
}
