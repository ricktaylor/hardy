use alloc::{borrow::Cow, boxed::Box, string::ToString, sync::Arc};
use core::ops::Range;

use hardy_cbor::{
    decode::FromCbor,
    encode::{Array, Encoder, ToCbor, emit},
};
use hmac::{KeyInit, Mac};

use super::{ScopeFlags, canonical_primary, key_wrap, rand_bytes};
use crate::{
    CaptureFieldErr, HashMap, block,
    bpsec::{Context, Error, bib, key, parse},
    eid,
};
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

#[derive(Debug)]
pub struct Results(pub Box<[u8]>);

impl Results {
    fn from_cbor(results: HashMap<u64, Range<usize>>, data: &[u8]) -> Result<Self, Error> {
        let mut r = None;
        for (id, range) in results {
            match id {
                1 => r = Some(parse::decode_box(range, data)?),
                _ => return Err(Error::InvalidContextResult(id)),
            }
        }

        Ok(Self(r.ok_or(Error::InvalidContextResult(1))?))
    }
}

impl ToCbor for Results {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit(&[&(1, &hardy_cbor::encode::Bytes(&self.0))]);
    }
}

/// Incremental verifier for one HMAC-SHA2 payload-target operation.
///
/// Created by [`Operation::begin_verify`] with every header-resident IPPT
/// part already absorbed; the caller feeds the target's block-type-specific
/// data through [`update`](Self::update) as it streams past, then settles
/// the operation with [`finish`](Self::finish).
///
/// The verifier owns everything it needs — including a copy of the resolved
/// content-encryption key inside the MAC state — so it is deliberately
/// `Send` and may cross `await` points and task boundaries: the streamed
/// target's bytes are not resident, so the keyed state must live for the
/// duration of the drain. This is a recorded exception to the header pass's
/// no-key-material-across-awaits rule.
#[must_use = "an unfinished verifier is an unchecked integrity statement — call finish()"]
pub struct Verifier {
    mac: MacInner,
    expected: Box<[u8]>,
}

enum MacInner {
    S256(hmac::Hmac<sha2::Sha256>),
    S384(hmac::Hmac<sha2::Sha384>),
    S512(hmac::Hmac<sha2::Sha512>),
}

impl MacInner {
    fn new(variant: ShaVariant, key: &[u8]) -> Result<Self, Error> {
        match variant {
            ShaVariant::HMAC_256_256 => Ok(Self::S256(
                hmac::Hmac::new_from_slice(key).map_err(|e| Error::Algorithm(e.to_string()))?,
            )),
            ShaVariant::HMAC_384_384 => Ok(Self::S384(
                hmac::Hmac::new_from_slice(key).map_err(|e| Error::Algorithm(e.to_string()))?,
            )),
            ShaVariant::HMAC_512_512 => Ok(Self::S512(
                hmac::Hmac::new_from_slice(key).map_err(|e| Error::Algorithm(e.to_string()))?,
            )),
            ShaVariant::Unrecognised(_) => Err(Error::UnsupportedOperation),
        }
    }

    fn update(&mut self, bytes: &[u8]) {
        match self {
            Self::S256(mac) => mac.update(bytes),
            Self::S384(mac) => mac.update(bytes),
            Self::S512(mac) => mac.update(bytes),
        }
    }

    // Constant-time comparison against an expected tag.
    fn verify_tag(self, expected: &[u8]) -> bool {
        match self {
            Self::S256(mac) => mac.verify_slice(expected).is_ok(),
            Self::S384(mac) => mac.verify_slice(expected).is_ok(),
            Self::S512(mac) => mac.verify_slice(expected).is_ok(),
        }
    }

    // The finished tag, for signing.
    fn finalize_tag(self) -> Box<[u8]> {
        match self {
            Self::S256(mac) => Box::from(mac.finalize().into_bytes().as_ref()),
            Self::S384(mac) => Box::from(mac.finalize().into_bytes().as_ref()),
            Self::S512(mac) => Box::from(mac.finalize().into_bytes().as_ref()),
        }
    }
}

impl Verifier {
    /// Absorb the next run of the target's block-type-specific data.
    pub fn update(&mut self, bytes: &[u8]) {
        self.mac.update(bytes);
    }

    /// Settle the operation: every byte of the target's data has been
    /// absorbed. Fails with [`Error::IntegrityCheckFailed`] on tag mismatch
    /// (constant-time comparison).
    pub fn finish(self) -> Result<(), Error> {
        if self.mac.verify_tag(&self.expected) {
            Ok(())
        } else {
            Err(Error::IntegrityCheckFailed)
        }
    }
}

// Absorb a fully-resident target's byte-string head and body: the target's
// canonical form for a primary-block target (RFC 9172 §4), its
// block-type-specific data otherwise. The head is sized from the resident
// payload's own length, not the block's parsed extent — the editor's
// in-flight template carries a placeholder `Block.data` range. Shared by
// `sign` and the verify wrapper's `Verifier::update_resident`.
fn absorb_resident_target(mac: &mut MacInner, args: &bib::OperationArgs) -> Result<(), Error> {
    let (target_block, payload) = args
        .blocks
        .block(args.target)
        .ok_or(Error::MissingSecurityTarget)?;
    let payload = payload.ok_or(Error::MissingSecurityTarget)?;
    let bytes: Cow<[u8]> = if matches!(target_block.block_type, block::Type::Primary) {
        canonical_primary(payload.as_ref())?
    } else {
        Cow::Borrowed(payload.as_ref())
    };
    mac.update(&emit(&hardy_cbor::encode::BytesHeader(bytes.len() as u64)).0);
    mac.update(&bytes);
    Ok(())
}

// Absorb the IPPT parts that precede the target's byte-string: the scope
// flags and the optional header parts (which RFC 9173 §3.7 omits for a
// primary-block target). The single source of the IPPT header rules; each
// caller then emits the target's byte-string head + body — the resident
// path in one step (`absorb_resident_target`), the streaming `begin_verify`
// path by emitting the head then feeding the body through `Verifier::update`.
fn ippt_prefix(
    mac: &mut MacInner,
    flags: &ScopeFlags,
    args: &bib::OperationArgs,
) -> Result<(), Error> {
    mac.update(
        &emit(&ScopeFlags {
            include_primary_block: flags.include_primary_block,
            include_target_header: flags.include_target_header,
            include_security_header: flags.include_security_header,
            ..Default::default()
        })
        .0,
    );

    let target_block = args
        .blocks
        .block_header(args.target)
        .ok_or(Error::MissingSecurityTarget)?;

    if !matches!(target_block.block_type, block::Type::Primary) {
        if flags.include_primary_block {
            let raw = args
                .blocks
                .block(0)
                .and_then(|v| v.1)
                .expect("Missing primary block!");
            // RFC 9172 §4: IPPT requires the canonical (deterministic) form.
            mac.update(&canonical_primary(raw.as_ref())?);
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
            .block_header(args.source)
            .ok_or(Error::MissingSecurityTarget)?;
        let mut encoder = Encoder::new();
        encoder.emit(&source_block.block_type);
        encoder.emit(&args.source);
        encoder.emit(&source_block.flags);
        mac.update(&encoder.build());
    }

    Ok(())
}

enum KeyWrap {
    Aes128,
    Aes192,
    Aes256,
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

        let active_cek = cek
            .as_ref()
            .map_or(kek.as_ref(), |cek: &zeroize::Zeroizing<Box<[u8]>>| {
                cek.as_ref()
            });

        let mut mac = MacInner::new(variant, active_cek)?;
        ippt_prefix(&mut mac, &scope_flags, &args)?;
        absorb_resident_target(&mut mac, &args)?;
        let results = Results(mac.finalize_tag());

        let key = if let (Some(cek), Some(key_wrap)) = (cek, key_wrap) {
            let key = match key_wrap {
                KeyWrap::Aes128 => key_wrap::wrap::<aes_kw::aes::Aes128>(kek.as_ref(), &cek),
                KeyWrap::Aes192 => key_wrap::wrap::<aes_kw::aes::Aes192>(kek.as_ref(), &cek),
                KeyWrap::Aes256 => key_wrap::wrap::<aes_kw::aes::Aes256>(kek.as_ref(), &cek),
            }
            .map_err(Error::Algorithm)?;
            Some(key.into())
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

    /// Begin incremental verification of this operation against a target
    /// whose block-type-specific data is not resident (the streaming
    /// ingress drain). Absorbs every header-resident IPPT part — the
    /// target's data length comes from its parsed extent — and returns the
    /// [`Verifier`] the caller feeds as the data streams past.
    ///
    /// The resolved key material is *copied* into the returned MAC state
    /// (see [`Verifier`] for the recorded key-handling exception).
    /// [`Error::NoKey`] means no usable key: the caller's policy skip.
    pub fn begin_verify<K>(
        &self,
        key_source: &K,
        args: &bib::OperationArgs,
    ) -> Result<Verifier, Error>
    where
        K: key::KeySource + ?Sized,
    {
        let mut mac = self.prepared_mac(key_source, args)?;

        // The streamed target's bytes are not resident, so the byte-string
        // head sizes from the parsed extent — the drain feeds exactly that
        // many bytes through `Verifier::update`. (The resident `verify` path
        // sizes its head from the payload's own length in
        // `absorb_resident_target`, because the editor's in-flight template
        // carries a placeholder `Block.data` range.)
        let target_block = args
            .blocks
            .block_header(args.target)
            .ok_or(Error::MissingSecurityTarget)?;
        let extent_len = target_block.data.end - target_block.data.start;
        mac.update(&emit(&hardy_cbor::encode::BytesHeader(extent_len)).0);

        Ok(Verifier {
            mac,
            expected: self.results.0.clone(),
        })
    }

    /// Verify a fully-resident target block. The all-in-one counterpart to
    /// [`begin_verify`](Self::begin_verify): it can't reuse that path
    /// (which sizes the byte-string head from the parsed extent and streams
    /// raw bytes — wrong for a primary-block target's canonical form, and
    /// for the editor's placeholder extent during signing), but it shares
    /// every other primitive — [`prepared_mac`](Self::prepared_mac) for the
    /// IPPT header, `absorb_resident_target` for the resident head + body,
    /// and [`Verifier::finish`] for the constant-time settle.
    pub fn verify<K>(&self, key_source: &K, args: &bib::OperationArgs) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
        let mut verifier = Verifier {
            mac: self.prepared_mac(key_source, args)?,
            expected: self.results.0.clone(),
        };
        absorb_resident_target(&mut verifier.mac, args)?;
        verifier.finish()
    }

    // The setup both verification paths share: resolve the key, build the
    // MAC, and absorb the IPPT header parts (`ippt_prefix`). Each path then
    // emits the target's byte-string head + body its own way — the one step
    // that legitimately differs (resident length vs streamed extent).
    fn prepared_mac<K>(&self, key_source: &K, args: &bib::OperationArgs) -> Result<MacInner, Error>
    where
        K: key::KeySource + ?Sized,
    {
        let cek = self.resolve_cek_owned(key_source, args.bpsec_source)?;
        let mut mac = MacInner::new(self.parameters.variant, &cek)?;
        ippt_prefix(&mut mac, &self.parameters.flags, args)?;
        Ok(mac)
    }

    // The single verification-key resolver, shared by `verify` and
    // `begin_verify`: the unwrapped CEK in key-wrap mode, a copy of the
    // KeySource's key in direct mode — owned, because a streaming verifier
    // outlives the KeySource borrow (the resident path pays one small key
    // copy for the shared code path).
    fn resolve_cek_owned<K>(
        &self,
        key_source: &K,
        bpsec_source: &eid::Eid,
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error>
    where
        K: key::KeySource + ?Sized,
    {
        if let Some(wrapped_cek) = &self.parameters.key {
            let jwk = key_source
                .key(
                    bpsec_source,
                    &[key::Operation::UnwrapKey, key::Operation::Verify],
                )
                .ok_or(Error::NoKey)?;

            if Some(self.parameters.variant) != as_variant(jwk.key_algorithm) {
                return Err(Error::IntegrityCheckFailed);
            }

            let key::Type::OctetSequence { key } = &jwk.key_type else {
                return Err(Error::IntegrityCheckFailed);
            };

            match as_key_wrap(jwk.key_algorithm) {
                Some(KeyWrap::Aes128) => {
                    key_wrap::unwrap::<aes_kw::aes::Aes128>(key.as_ref(), wrapped_cek)
                }
                Some(KeyWrap::Aes192) => {
                    key_wrap::unwrap::<aes_kw::aes::Aes192>(key.as_ref(), wrapped_cek)
                }
                Some(KeyWrap::Aes256) => {
                    key_wrap::unwrap::<aes_kw::aes::Aes256>(key.as_ref(), wrapped_cek)
                }
                None => return Err(Error::IntegrityCheckFailed),
            }
            .map(|cek| zeroize::Zeroizing::from(Box::from(cek)))
            .map_err(|_| Error::IntegrityCheckFailed)
        } else {
            let jwk = key_source
                .key(bpsec_source, &[key::Operation::Verify])
                .ok_or(Error::NoKey)?;

            if Some(self.parameters.variant) != as_variant(jwk.key_algorithm) {
                return Err(Error::IntegrityCheckFailed);
            }

            let key::Type::OctetSequence { key } = &jwk.key_type else {
                return Err(Error::IntegrityCheckFailed);
            };

            Ok(zeroize::Zeroizing::from(key.clone()))
        }
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
    let parameters = Arc::from(
        Parameters::from_cbor(asb.parameters, data)
            .map_field_err::<Error>("RFC9173 HMAC-SHA2 parameters")?,
    );

    // Unpack results
    let mut operations = HashMap::with_capacity(asb.results.len());
    for (target, results) in asb.results {
        operations.insert(
            target,
            bib::Operation::HMAC_SHA2(Operation {
                parameters: parameters.clone(),
                results: Results::from_cbor(results, data)
                    .map_field_err::<Error>("RFC9173 HMAC-SHA2 results")?,
            }),
        );
    }
    Ok((asb.source, operations))
}
