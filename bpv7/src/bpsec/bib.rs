use hardy_cbor::{
    decode::{FromCbor, parse},
    encode::{Array, Encoder, ToCbor},
};
use smallvec::SmallVec;

#[cfg(feature = "rfc9173")]
use crate::bpsec::rfc9173;
use crate::{
    HashMap, block,
    bpsec::{BlockSet, Context, Error, key, parse},
    crc, eid,
};
/// A parsed BIB (Block Integrity Block) security operation.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum Operation {
    /// HMAC-SHA2 integrity operation (RFC 9173).
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2(rfc9173::bib_hmac_sha2::Operation),
    /// An unrecognised security context (context ID, raw parameters/results).
    Unrecognised(u64, parse::UnknownOperation),
}

/// Arguments passed to a BIB verification operation.
pub struct OperationArgs<'a> {
    /// The EID of the security source that created this BIB.
    pub bpsec_source: &'a eid::Eid,
    /// The block number of the block being verified.
    pub target: u64,
    /// The block number of the BIB itself.
    pub source: u64,
    /// A view of the bundle's blocks for accessing related data during verification.
    pub blocks: &'a dyn BlockSet<'a>,
}

/// Incremental verifier for one BIB operation. Context-dispatching wrapper
/// over the per-context verifiers; the single verification engine — the
/// all-in-one [`Operation::verify`] is a thin resident-target wrapper over
/// it, and the streaming ingress drain feeds it a non-resident payload
/// target segment by segment.
///
/// Owns everything it needs (including copied key material — see the
/// per-context verifier docs), so it is `Send` and may cross `await`
/// points and task boundaries.
///
/// Contract for future security contexts: a verifier carries the *minimum
/// derived state* across the drain — a running digest, never a raw key
/// larger than the digest state. Resolve key material from the
/// [`KeySource`](super::key::KeySource) inside `begin_verify`'s sync scope;
/// a context that instead needs the key at settle (a hash-then-verify
/// signature scheme, say) should extend [`finish`](Self::finish) to take a
/// `KeySource` — the settle site is sync and can re-resolve — rather than
/// store the key in the verifier.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[must_use = "an unfinished verifier is an unchecked integrity statement — call finish()"]
pub enum Verifier {
    /// HMAC-SHA2 incremental verification (RFC 9173).
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2(rfc9173::bib_hmac_sha2::Verifier),
}

impl Verifier {
    /// Absorb the next run of the target's block-type-specific data.
    #[allow(unused_variables)]
    pub fn update(&mut self, bytes: &[u8]) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(v) => v.update(bytes),
            // With no security context compiled in the enum is empty and a
            // `Verifier` is never constructed; the arm keeps the reference
            // match exhaustive.
            #[cfg(not(feature = "rfc9173"))]
            _ => unreachable!("no security context compiled in"),
        }
    }

    /// Settle the operation once every byte has been absorbed. Fails with
    /// [`Error::IntegrityCheckFailed`] on tag mismatch.
    pub fn finish(self) -> Result<(), Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(v) => v.finish(),
            #[cfg(not(feature = "rfc9173"))]
            _ => unreachable!("no security context compiled in"),
        }
    }
}

impl Operation {
    /// Returns `true` if this operation uses an unrecognised security context.
    pub fn is_unsupported(&self) -> bool {
        self.unsupported_error().is_some()
    }

    /// The error describing why this operation is unsupported:
    /// [`Error::UnrecognisedContext`] for an unrecognised security context
    /// id, [`Error::UnsupportedOperation`] for a recognised context with
    /// unrecognised parameters. `None` when the operation is supported.
    pub fn unsupported_error(&self) -> Option<Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(operation) => operation
                .is_unsupported()
                .then_some(Error::UnsupportedOperation),
            Self::Unrecognised(id, ..) => Some(Error::UnrecognisedContext(*id)),
        }
    }

    /// Begin incremental verification of this operation: the returned
    /// [`Verifier`] absorbs the target's data streamed through
    /// [`Verifier::update`] (the ingress drain); a resident target takes the
    /// all-in-one [`verify`](Self::verify) instead. Applies the RFC 9172
    /// Section 3.8 CRC-presence rule; [`Error::NoKey`] is the caller's
    /// policy skip.
    #[allow(unused_variables)]
    pub fn begin_verify<K>(&self, key_source: &K, args: &OperationArgs) -> Result<Verifier, Error>
    where
        K: key::KeySource + ?Sized,
    {
        // RFC 9172 Section 3.8: CRC must be removed for targets "other than
        // the bundle's primary block". The primary block (block 0) is exempt.
        if args.target != 0
            && let Some(target_block) = args.blocks.block_header(args.target)
            && !matches!(target_block.crc_type, crc::CrcType::None)
        {
            return Err(Error::CrcPresent);
        }

        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.begin_verify(key_source, args).map(Verifier::HMAC_SHA2),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    /// Verifies the integrity of a fully-resident target block. The
    /// all-in-one counterpart to [`begin_verify`](Self::begin_verify);
    /// both share the per-context IPPT/MAC primitives. Applies the RFC 9172
    /// Section 3.8 CRC-presence rule; [`Error::NoKey`] is the caller's
    /// policy skip.
    #[allow(unused_variables)]
    pub fn verify<K>(&self, key_source: &K, args: OperationArgs) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
        // RFC 9172 Section 3.8: CRC must be removed for targets "other than
        // the bundle's primary block". The primary block (block 0) is exempt.
        if args.target != 0
            && let Some(target_block) = args.blocks.block_header(args.target)
            && !matches!(target_block.crc_type, crc::CrcType::None)
        {
            return Err(Error::CrcPresent);
        }

        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.verify(key_source, &args),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    fn emit_context(&self, encoder: &mut Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(&self, array: &mut Array) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array),
        }
    }
}

/// A set of BIB operations sharing a common security source.
///
/// Fields are crate-private: an `OperationSet` is only ever produced by the
/// parser or by `Signer`, both of which guarantee it is non-empty (`to_cbor`
/// relies on that invariant). External code builds BIBs via `Signer` and reads
/// via the [`source`](Self::source)/[`operations`](Self::operations) accessors.
#[derive(Debug)]
pub struct OperationSet {
    // The EID of the security source.
    pub(crate) source: eid::Eid,
    // Operations keyed by target block number.
    pub(crate) operations: HashMap<u64, Operation>,
}

impl OperationSet {
    /// The EID of the security source.
    #[inline]
    pub fn source(&self) -> &eid::Eid {
        &self.source
    }

    /// The operations in this set, keyed by target block number.
    #[inline]
    pub fn operations(&self) -> &HashMap<u64, Operation> {
        &self.operations
    }

    /// Returns `true` if any operation in this set uses an unrecognised context.
    pub fn is_unsupported(&self) -> bool {
        self.operations.values().any(|op| op.is_unsupported())
    }

    /// The error describing why this set is unsupported (the first
    /// unsupported operation's [`Operation::unsupported_error`]), or `None`
    /// when every operation is supported.
    pub fn unsupported_error(&self) -> Option<Error> {
        self.operations
            .values()
            .find_map(|op| op.unsupported_error())
    }

    /// Per-OperationSet structural validation of this BIB against the
    /// bundle's blocks: every target must exist (RFC 9172 §3.6) and not be
    /// a security block (§3.9, mirrored to also reject targeting a BIB),
    /// no target may already be covered by a different BIB (§2.6), and a
    /// target that is BCB-encrypted requires this BIB to be BCB-encrypted
    /// too (§3.9). Pure inspection — stamps no coverage; the caller stamps
    /// after a successful return.
    ///
    /// §3.8 (a BCB targeting a BIB must share a target with it) is not
    /// checked here — it fires only for BCB-encrypted BIBs whose
    /// OperationSet can't be decoded without keys. Shared by the
    /// structural parser ([`crate::parse`]) and the keyed
    /// [`crate::checks::verify`] pass as the single source of truth for
    /// the per-OperationSet BIB rules.
    ///
    /// # Panics
    ///
    /// Panics if `bib_block_number` is not in `blocks`. The caller looked
    /// the BIB up in `blocks` to obtain this OperationSet; passing a
    /// different block set is a caller error, not a recoverable state.
    pub fn check<'a, B>(&self, bib_block_number: u64, blocks: &'a B) -> Result<(), Error>
    where
        B: BlockSet<'a> + ?Sized,
    {
        // Whether this BIB is itself protected by a BCB — used by the §3.9
        // check on each target.
        let bib_bcb = blocks
            .block_header(bib_block_number)
            .expect("OperationSet::check called with a bib_block_number not in the block set")
            .bcb;

        for &target_number in self.operations.keys() {
            let target_block = blocks
                .block_header(target_number)
                .ok_or(Error::MissingSecurityTarget)?;
            if matches!(
                target_block.block_type,
                block::Type::BlockSecurity | block::Type::BlockIntegrity
            ) {
                return Err(Error::InvalidBIBTarget);
            }
            if matches!(target_block.bib, block::BibCoverage::Some(n) if n != bib_block_number) {
                return Err(Error::DuplicateOpTarget);
            }
            if target_block.bcb.is_some() && bib_bcb.is_none() {
                return Err(Error::BIBMustBeEncrypted);
            }
        }
        Ok(())
    }
}

impl ToCbor for OperationSet {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        // Ensure we process operations in the same order
        let (targets, operations): (SmallVec<[&u64; 4]>, SmallVec<[&Operation; 4]>) =
            self.operations.iter().unzip();

        // Targets
        encoder.emit(targets.as_slice());

        // Context
        operations
            .first()
            // SAFETY: An OperationSet is non-empty by construction
            .expect("OperationSet must contain at least one operation")
            .emit_context(encoder, &self.source);

        // Results
        encoder.emit_array(Some(operations.len()), |a| {
            for op in operations {
                op.emit_result(a);
            }
        });
    }
}

impl FromCbor for OperationSet {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        // ASB parsing is strict-canonical (errors on non-shortest, indefinite,
        // or tagged content) and likewise the rfc9173 context parsers below,
        // so any value returned here is canonical by construction.
        let (asb, len) = parse::<(parse::AbstractSyntaxBlock, usize)>(data)?;

        // Unpack into strong types
        #[allow(unreachable_patterns)]
        match asb.context {
            #[cfg(feature = "rfc9173")]
            Context::BIB_HMAC_SHA2 => rfc9173::bib_hmac_sha2::parse(asb, data)
                .map(|(source, operations)| (OperationSet { source, operations }, true, len)),
            Context::Unrecognised(id) => {
                parse::UnknownOperation::parse(asb, data).map(|(source, operations)| {
                    (
                        OperationSet {
                            source,
                            operations: operations
                                .into_iter()
                                .map(|(t, o)| (t, Operation::Unrecognised(id, o)))
                                .collect(),
                        },
                        true,
                        len,
                    )
                })
            }
            c => Err(Error::InvalidContext(c)),
        }
    }
}
