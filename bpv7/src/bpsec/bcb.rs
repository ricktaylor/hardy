use super::*;
use smallvec::SmallVec;

/// A parsed BCB (Block Confidentiality Block) security operation.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum Operation {
    /// AES-GCM encryption operation (RFC 9173).
    #[cfg(feature = "rfc9173")]
    AES_GCM(rfc9173::bcb_aes_gcm::Operation),
    /// An unrecognised security context (context ID, raw parameters/results).
    Unrecognised(u64, parse::UnknownOperation),
}

/// Arguments passed to a BCB decryption operation.
pub struct OperationArgs<'a> {
    /// The EID of the security source that created this BCB.
    pub bpsec_source: &'a eid::Eid,
    /// The block number of the block being decrypted.
    pub target: u64,
    /// The block number of the BCB itself.
    pub source: u64,
    /// A view of the bundle's blocks for accessing related data during decryption.
    pub blocks: &'a dyn BlockSet<'a>,
}

impl Operation {
    /// Returns `true` if this operation uses an unrecognised security context.
    pub fn is_unsupported(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(operation) => operation.is_unsupported(),
            Self::Unrecognised(..) => true,
        }
    }

    /// Returns true if multiple security operations can share the same security
    /// context parameters (and thus be in the same BCB).
    ///
    /// BCB-AES-GCM (RFC 9173) returns false because each encryption requires a
    /// unique IV, which is stored in the context parameters.
    ///
    /// Future contexts (e.g., COSE-based) may return true if they store per-target
    /// IVs in the results rather than shared parameters. The operation instance
    /// can inspect its parameters to determine this.
    pub fn can_share(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(_) => false, // IV in context parameters requires separate blocks
            // Unknown contexts: assume they cannot share (conservative default)
            Self::Unrecognised(..) => false,
        }
    }

    /// Decrypts the target block using the provided key source.
    #[allow(unused_variables)]
    pub fn decrypt<K>(
        &self,
        key_source: &K,
        args: OperationArgs,
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error>
    where
        K: key::KeySource + ?Sized,
    {
        // RFC 9172 Section 3.9: CRC must be removed from BCB targets.
        // Note: BCBs cannot target the primary block, so no block 0 exemption needed.
        if let Some((target_block, _)) = args.blocks.block(args.target)
            && !matches!(target_block.crc_type, crc::CrcType::None)
        {
            return Err(Error::CrcPresent);
        }

        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(op) => op.decrypt(key_source, args),
            Self::Unrecognised(id, ..) => Err(Error::UnrecognisedContext(*id)),
        }
    }

    fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array),
        }
    }
}

/// A set of BCB operations sharing a common security source.
#[derive(Debug)]
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

    /// Returns true if this BCB's context allows multiple targets to share
    /// security context parameters.
    pub fn can_share(&self) -> bool {
        // All operations in a set share the same context, so check any one
        self.operations
            .values()
            .next()
            .is_some_and(|op| op.can_share())
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
            Context::BCB_AES_GCM => {
                rfc9173::bcb_aes_gcm::parse(asb, data).map(|(source, operations, s)| {
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
