use super::*;
use smallvec::SmallVec;

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

impl Operation {
    /// Returns `true` if this operation uses an unrecognised security context.
    pub fn is_unsupported(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(operation) => operation.is_unsupported(),
            Self::Unrecognised(..) => true,
        }
    }

    /// Verifies the integrity of the target block using the provided key source.
    #[allow(unused_variables)]
    pub fn verify<K>(&self, key_source: &K, args: OperationArgs) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
        // RFC 9172 Section 3.8: CRC must be removed for targets "other than the bundle's
        // primary block". The primary block (block 0) is exempt from this requirement.
        if args.target != 0
            && let Some((target_block, _)) = args.blocks.block(args.target)
            && !matches!(target_block.crc_type, crc::CrcType::None)
        {
            return Err(Error::CrcPresent);
        }

        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.verify(key_source, args),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
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

impl hardy_cbor::encode::ToCbor for OperationSet {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
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

impl hardy_cbor::decode::FromCbor for OperationSet {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        // ASB parsing is strict-canonical (errors on non-shortest, indefinite,
        // or tagged content) and likewise the rfc9173 context parsers below,
        // so any value returned here is canonical by construction.
        let (asb, len) = hardy_cbor::decode::parse::<(parse::AbstractSyntaxBlock, usize)>(data)?;

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
