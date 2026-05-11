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
pub struct OperationSet {
    /// The EID of the security source.
    pub source: eid::Eid,
    /// Operations keyed by target block number.
    pub operations: HashMap<u64, Operation>,
}

impl OperationSet {
    /// Returns `true` if any operation in this set uses an unrecognised context.
    pub fn is_unsupported(&self) -> bool {
        self.operations.values().any(|op| op.is_unsupported())
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
            .unwrap()
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
        let (asb, shortest, len) =
            hardy_cbor::decode::parse::<(parse::AbstractSyntaxBlock, bool, usize)>(data)?;

        // Unpack into strong types
        #[allow(unreachable_patterns)]
        match asb.context {
            #[cfg(feature = "rfc9173")]
            Context::BIB_HMAC_SHA2 => {
                rfc9173::bib_hmac_sha2::parse(asb, data).map(|(source, operations, s)| {
                    (OperationSet { source, operations }, shortest && s, len)
                })
            }
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
                        shortest,
                        len,
                    )
                })
            }
            c => Err(Error::InvalidContext(c)),
        }
    }
}
