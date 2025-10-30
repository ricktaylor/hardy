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

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ShaVariant {
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
struct Parameters {
    variant: ShaVariant,
    key: Option<Box<[u8]>>,
    flags: ScopeFlags,
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
                    result.variant = hardy_cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?;
                }
                2 => {
                    result.key = Some(parse::decode_box(range, data).map(|(v, s)| {
                        shortest = shortest && s;
                        v
                    })?);
                }
                3 => {
                    result.flags = hardy_cbor::decode::parse(&data[range.start..range.end]).map(
                        |(v, s)| {
                            shortest = shortest && s;
                            v
                        },
                    )?;
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
struct Results(Box<[u8]>);

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

    let target_block = args
        .blocks
        .block(args.target)
        .ok_or(Error::MissingSecurityTarget)?;

    if !matches!(target_block.block_type, block::Type::Primary) {
        if flags.include_primary_block {
            mac.update(args.blocks.block_payload(0).expect("No primary block!"));
        }

        if flags.include_target_header {
            let mut encoder = hardy_cbor::encode::Encoder::new();
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
            .ok_or(Error::MissingSecurityTarget)?;

        let mut encoder = hardy_cbor::encode::Encoder::new();
        encoder.emit(&source_block.block_type);
        encoder.emit(&args.source);
        encoder.emit(&source_block.flags);
        mac.update(&encoder.build());
    }

    mac.update(
        args.blocks
            .block_payload(args.target)
            .ok_or(Error::MissingSecurityTarget)?,
    );

    Ok(mac.finalize().into_bytes())
}

#[derive(Debug)]
pub struct Operation {
    parameters: Rc<Parameters>,
    results: Results,
}

impl Operation {
    pub fn is_unsupported(&self) -> bool {
        matches!(self.parameters.variant, ShaVariant::Unrecognised(_))
    }

    pub fn protects_primary_block(&self) -> bool {
        self.parameters.flags.include_primary_block
    }

    pub fn sign(jwk: &Key, args: bib::OperationArgs) -> Result<Option<Self>, Error> {
        if let Some(ops) = &jwk.operations
            && !ops.contains(&key::Operation::Sign)
        {
            return Ok(None);
        }

        let (cek, variant) = match &jwk.key_algorithm {
            Some(key::KeyAlgorithm::HS256_A128KW) => {
                if let Some(ops) = &jwk.operations
                    && !ops.contains(&key::Operation::WrapKey)
                {
                    return Ok(None);
                }
                (
                    Some(rand_key(Box::from([0u8; 32]))?),
                    ShaVariant::HMAC_256_256,
                )
            }
            Some(key::KeyAlgorithm::A128KW)
            | Some(key::KeyAlgorithm::A192KW)
            | Some(key::KeyAlgorithm::A256KW)
            | Some(key::KeyAlgorithm::HS384_A192KW) => {
                if let Some(ops) = &jwk.operations
                    && !ops.contains(&key::Operation::WrapKey)
                {
                    return Ok(None);
                }
                (
                    Some(rand_key(Box::from([0u8; 48]))?),
                    ShaVariant::HMAC_384_384,
                )
            }
            Some(key::KeyAlgorithm::HS512_A256KW) => {
                if let Some(ops) = &jwk.operations
                    && !ops.contains(&key::Operation::WrapKey)
                {
                    return Ok(None);
                }
                (
                    Some(rand_key(Box::from([0u8; 64]))?),
                    ShaVariant::HMAC_512_512,
                )
            }
            Some(key::KeyAlgorithm::HS256) => (None, ShaVariant::HMAC_256_256),
            Some(key::KeyAlgorithm::HS384) => (None, ShaVariant::HMAC_384_384),
            Some(key::KeyAlgorithm::HS512) => (None, ShaVariant::HMAC_512_512),
            _ => {
                return Ok(None);
            }
        };

        let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
            return Ok(None);
        };

        let flags = ScopeFlags::default();
        let (results, key) = if let Some(cek) = cek {
            let key = match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::A128KW) | Some(key::KeyAlgorithm::HS256_A128KW) => {
                    aes_kw::KekAes128::try_from(kek.as_ref())
                        .and_then(|kek| kek.wrap_vec(&cek))
                        .map_err(|e| Error::Algorithm(e.to_string()))
                }
                Some(key::KeyAlgorithm::A192KW) | Some(key::KeyAlgorithm::HS384_A192KW) => {
                    aes_kw::KekAes192::try_from(kek.as_ref())
                        .and_then(|kek| kek.wrap_vec(&cek))
                        .map_err(|e| Error::Algorithm(e.to_string()))
                }
                Some(key::KeyAlgorithm::A256KW) | Some(key::KeyAlgorithm::HS512_A256KW) => {
                    aes_kw::KekAes256::try_from(kek.as_ref())
                        .and_then(|kek| kek.wrap_vec(&cek))
                        .map_err(|e| Error::Algorithm(e.to_string()))
                }
                _ => return Ok(None),
            }?;

            (
                Results(match variant {
                    ShaVariant::HMAC_256_256 => {
                        (*calculate_hmac::<sha2::Sha256>(&flags, &cek, &args)?).into()
                    }
                    ShaVariant::HMAC_384_384 => {
                        (*calculate_hmac::<sha2::Sha384>(&flags, &cek, &args)?).into()
                    }
                    ShaVariant::HMAC_512_512 => {
                        (*calculate_hmac::<sha2::Sha512>(&flags, &cek, &args)?).into()
                    }
                    ShaVariant::Unrecognised(_) => unreachable!(),
                }),
                Some(key.into()),
            )
        } else {
            (
                Results(match variant {
                    ShaVariant::HMAC_256_256 => {
                        (*calculate_hmac::<sha2::Sha256>(&flags, kek, &args)?).into()
                    }
                    ShaVariant::HMAC_384_384 => {
                        (*calculate_hmac::<sha2::Sha384>(&flags, kek, &args)?).into()
                    }
                    ShaVariant::HMAC_512_512 => {
                        (*calculate_hmac::<sha2::Sha512>(&flags, kek, &args)?).into()
                    }
                    ShaVariant::Unrecognised(_) => unreachable!(),
                }),
                None,
            )
        };

        Ok(Some(Self {
            parameters: Rc::new(Parameters {
                variant,
                key,
                flags,
            }),
            results,
        }))
    }

    pub fn verify_any(
        &self,
        key_f: &impl key::KeyStore,
        args: bib::OperationArgs,
    ) -> Result<Option<bool>, Error> {
        if let Some(cek) = &self.parameters.key {
            for jwk in key_f.decrypt_keys(
                args.bpsec_source,
                &[key::Operation::UnwrapKey, key::Operation::Verify],
            ) {
                if let key::Type::OctetSequence { key: kek } = &jwk.key_type {
                    match &jwk.key_algorithm {
                        Some(key::KeyAlgorithm::HS256_A128KW)
                            if self.parameters.variant != ShaVariant::HMAC_256_256 =>
                        {
                            continue;
                        }
                        Some(key::KeyAlgorithm::HS384_A192KW)
                            if self.parameters.variant != ShaVariant::HMAC_384_384 =>
                        {
                            continue;
                        }
                        Some(key::KeyAlgorithm::HS512_A256KW)
                            if self.parameters.variant != ShaVariant::HMAC_512_512 =>
                        {
                            continue;
                        }
                        _ => {}
                    }

                    if let Some(cek) = match &jwk.key_algorithm {
                        Some(key::KeyAlgorithm::A128KW) | Some(key::KeyAlgorithm::HS256_A128KW) => {
                            aes_kw::KekAes128::try_from(kek.as_ref())
                                .and_then(|kek| kek.unwrap_vec(cek))
                                .ok()
                        }
                        Some(key::KeyAlgorithm::A192KW) | Some(key::KeyAlgorithm::HS384_A192KW) => {
                            aes_kw::KekAes192::try_from(kek.as_ref())
                                .and_then(|kek| kek.unwrap_vec(cek))
                                .ok()
                        }
                        Some(key::KeyAlgorithm::A256KW) | Some(key::KeyAlgorithm::HS512_A256KW) => {
                            aes_kw::KekAes256::try_from(kek.as_ref())
                                .and_then(|kek| kek.unwrap_vec(cek))
                                .ok()
                        }
                        _ => None,
                    }
                    .map(|v| zeroize::Zeroizing::from(Box::from(v)))
                        && match self.parameters.variant {
                            ShaVariant::HMAC_256_256 => {
                                calculate_hmac::<sha2::Sha256>(&self.parameters.flags, &cek, &args)
                                    .ok()
                                    .map(|mac| *mac == *self.results.0)
                            }
                            ShaVariant::HMAC_384_384 => {
                                calculate_hmac::<sha2::Sha384>(&self.parameters.flags, &cek, &args)
                                    .ok()
                                    .map(|mac| *mac == *self.results.0)
                            }
                            ShaVariant::HMAC_512_512 => {
                                calculate_hmac::<sha2::Sha512>(&self.parameters.flags, &cek, &args)
                                    .ok()
                                    .map(|mac| *mac == *self.results.0)
                            }
                            ShaVariant::Unrecognised(_) => return Err(Error::UnsupportedOperation),
                        } == Some(true)
                    {
                        return Ok(Some(true));
                    }
                }
            }
        } else {
            for jwk in key_f.decrypt_keys(args.bpsec_source, &[key::Operation::Verify]) {
                if let key::Type::OctetSequence { key: cek } = &jwk.key_type
                    && match (self.parameters.variant, &jwk.key_algorithm) {
                        (ShaVariant::HMAC_256_256, Some(key::KeyAlgorithm::HS256)) => {
                            calculate_hmac::<sha2::Sha256>(&self.parameters.flags, cek, &args)
                                .ok()
                                .map(|mac| *mac == *self.results.0)
                        }
                        (ShaVariant::HMAC_384_384, Some(key::KeyAlgorithm::HS384)) => {
                            calculate_hmac::<sha2::Sha384>(&self.parameters.flags, cek, &args)
                                .ok()
                                .map(|mac| *mac == *self.results.0)
                        }
                        (ShaVariant::HMAC_512_512, Some(key::KeyAlgorithm::HS512)) => {
                            calculate_hmac::<sha2::Sha512>(&self.parameters.flags, cek, &args)
                                .ok()
                                .map(|mac| *mac == *self.results.0)
                        }
                        (ShaVariant::Unrecognised(_), _) => {
                            return Err(Error::UnsupportedOperation);
                        }
                        _ => None,
                    } == Some(true)
                {
                    return Ok(Some(true));
                }
            }
        }

        Ok(None)
    }

    /// Will succeed if there is a valid key AND the verification passes
    pub fn verify(&self, jwk: &Key, args: bib::OperationArgs) -> Result<bool, Error> {
        if let Some(cek) = &self.parameters.key {
            let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
                return Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone()));
            };

            match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::HS256_A128KW)
                    if self.parameters.variant != ShaVariant::HMAC_256_256 =>
                {
                    return Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone()));
                }
                Some(key::KeyAlgorithm::HS384_A192KW)
                    if self.parameters.variant != ShaVariant::HMAC_384_384 =>
                {
                    return Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone()));
                }
                Some(key::KeyAlgorithm::HS512_A256KW)
                    if self.parameters.variant != ShaVariant::HMAC_512_512 =>
                {
                    return Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone()));
                }
                _ => {}
            }

            // Unwrap the key
            let cek = match &jwk.key_algorithm {
                Some(key::KeyAlgorithm::A128KW) | Some(key::KeyAlgorithm::HS256_A128KW) => {
                    aes_kw::KekAes128::try_from(kek.as_ref())
                        .and_then(|kek| kek.unwrap_vec(cek))
                        .map_err(|e| Error::Algorithm(e.to_string()))
                }
                Some(key::KeyAlgorithm::A192KW) | Some(key::KeyAlgorithm::HS384_A192KW) => {
                    aes_kw::KekAes192::try_from(kek.as_ref())
                        .and_then(|kek| kek.unwrap_vec(cek))
                        .map_err(|e| Error::Algorithm(e.to_string()))
                }
                Some(key::KeyAlgorithm::A256KW) | Some(key::KeyAlgorithm::HS512_A256KW) => {
                    aes_kw::KekAes256::try_from(kek.as_ref())
                        .and_then(|kek| kek.unwrap_vec(cek))
                        .map_err(|e| Error::Algorithm(e.to_string()))
                }
                _ => Err(Error::InvalidKey(key::Operation::UnwrapKey, jwk.clone())),
            }
            .map(|v| zeroize::Zeroizing::from(Box::from(v)))?;

            // And then HMAC
            match self.parameters.variant {
                ShaVariant::HMAC_256_256 => Ok(*calculate_hmac::<sha2::Sha256>(
                    &self.parameters.flags,
                    &cek,
                    &args,
                )? == *self.results.0),
                ShaVariant::HMAC_384_384 => Ok(*calculate_hmac::<sha2::Sha384>(
                    &self.parameters.flags,
                    &cek,
                    &args,
                )? == *self.results.0),
                ShaVariant::HMAC_512_512 => Ok(*calculate_hmac::<sha2::Sha512>(
                    &self.parameters.flags,
                    &cek,
                    &args,
                )? == *self.results.0),
                ShaVariant::Unrecognised(_) => Err(Error::UnsupportedOperation),
            }
        } else {
            let key::Type::OctetSequence { key: cek } = &jwk.key_type else {
                return Err(Error::InvalidKey(key::Operation::Verify, jwk.clone()));
            };

            match (self.parameters.variant, &jwk.key_algorithm) {
                (ShaVariant::HMAC_256_256, Some(key::KeyAlgorithm::HS256)) => Ok(
                    *calculate_hmac::<sha2::Sha256>(&self.parameters.flags, cek, &args)?
                        == *self.results.0,
                ),
                (ShaVariant::HMAC_384_384, Some(key::KeyAlgorithm::HS384)) => Ok(
                    *calculate_hmac::<sha2::Sha384>(&self.parameters.flags, cek, &args)?
                        == *self.results.0,
                ),
                (ShaVariant::HMAC_512_512, Some(key::KeyAlgorithm::HS512)) => Ok(
                    *calculate_hmac::<sha2::Sha512>(&self.parameters.flags, cek, &args)?
                        == *self.results.0,
                ),
                (ShaVariant::Unrecognised(_), _) => Err(Error::UnsupportedOperation),
                _ => Err(Error::InvalidKey(key::Operation::Verify, jwk.clone())),
            }
        }
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
