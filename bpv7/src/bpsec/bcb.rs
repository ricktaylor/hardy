use hardy_cbor::{
    decode::{FromCbor, parse},
    encode::{Array, Encoder, ToCbor},
};
use smallvec::SmallVec;

#[cfg(feature = "rfc9173")]
use crate::bpsec::rfc9173;
use crate::{
    HashMap, block,
    bpsec::{Context, Error, parse},
    eid,
    reader::Reader,
};

#[cfg(feature = "bpsec")]
mod keyed;
#[cfg(feature = "bpsec")]
pub use keyed::Decryptor;
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
    pub source: &'a eid::Eid,
    /// The block number of the block being decrypted.
    pub target: u64,
    /// The block number of the BCB itself.
    pub security_block: u64,
    /// A view of the bundle's blocks for accessing related data during decryption.
    pub blocks: &'a dyn Reader<'a>,
}

impl Operation {
    /// Whether this node cannot process the operation, its security context
    /// being one it does not support.
    pub fn is_unsupported(&self) -> bool {
        self.unsupported_error().is_some()
    }

    /// The error describing why this operation is unsupported:
    /// [`Error::UnrecognisedContext`] for an unrecognised security context
    /// id, [`Error::UnsupportedOperation`] for a recognised context with
    /// unrecognised parameters. `None` when the operation is supported.
    pub(crate) fn unsupported_error(&self) -> Option<Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(operation) => operation
                .is_unsupported()
                .then_some(Error::UnsupportedOperation),
            Self::Unrecognised(id, ..) => Some(Error::UnrecognisedContext(*id)),
        }
    }

    /// Returns `true` if this operation's input includes the primary block
    /// through its scope, so any change to the primary block's bytes
    /// invalidates it, as under RFC 9173's default scope. An unrecognised
    /// context's parameters cannot be read, so it is taken to include the
    /// primary.
    ///
    /// The answer is about the primary as part of another target's input:
    /// an operation whose target is the primary block covers it regardless.
    pub(crate) fn scope_includes_primary(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(operation) => operation.scope_includes_primary(),
            Self::Unrecognised(..) => true,
        }
    }

    fn emit_context(&self, encoder: &mut Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(&self, array: &mut Array) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array),
        }
    }
}

/// A set of BCB operations sharing a common security source.
///
/// Fields are crate-private: an `OperationSet` is only ever produced by the
/// parser or by `Encryptor`, both of which guarantee it is non-empty (`to_cbor`
/// relies on that invariant). External code builds BCBs via `Encryptor` and reads
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

    /// The error describing why this set is unsupported (the first
    /// unsupported operation's [`Operation::unsupported_error`]), or `None`
    /// when every operation is supported.
    pub(crate) fn unsupported_error(&self) -> Option<Error> {
        self.operations
            .values()
            .find_map(|op| op.unsupported_error())
    }

    /// Per-OperationSet structural validation of this BCB against the
    /// bundle's blocks (RFC 9172 §3.6 / §3.7): the BCB MUST NOT set
    /// delete-block-on-failure, every target must exist, no target may be
    /// the primary block or another security block, a payload target
    /// requires must-replicate, and no target may already be covered by a
    /// different BCB. Pure inspection — stamps no coverage; the caller
    /// stamps after a successful return. Cross-BCB uniqueness (§2.6) is
    /// not checked here (it needs every operation in view at once).
    ///
    /// Shared by the structural parser ([`crate::parse`]) and the keyed
    /// [`Parsed::check`](crate::parse::Parsed::check) pass as the single
    /// source of truth for the per-OperationSet BCB rules.
    ///
    /// # Panics
    ///
    /// Panics if `bcb_block_number` is not in `blocks`. The caller looked
    /// the BCB up in `blocks` to obtain this OperationSet; passing a
    /// different block set is a caller error, not a recoverable state.
    pub(crate) fn check<'a, B>(&self, bcb_block_number: u64, blocks: &'a B) -> Result<(), Error>
    where
        B: Reader<'a> + ?Sized,
    {
        let bcb_block = blocks
            .block_header(bcb_block_number)
            .expect("OperationSet::check called with a bcb_block_number not in the block set");
        if bcb_block.flags.delete_block_on_failure {
            return Err(Error::BCBDeleteFlag);
        }
        let must_replicate = bcb_block.flags.must_replicate;

        for &target_number in self.operations.keys() {
            let target_block = blocks
                .block_header(target_number)
                .ok_or(Error::MissingSecurityTarget)?;
            match target_block.block_type {
                block::Type::Primary | block::Type::BlockSecurity => {
                    return Err(Error::InvalidBCBTarget);
                }
                block::Type::Payload if !must_replicate => {
                    return Err(Error::BCBMustReplicate);
                }
                _ => {}
            }
            if matches!(target_block.bcb, Some(n) if n != bcb_block_number) {
                return Err(Error::DuplicateOpTarget);
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
            Context::BCB_AES_GCM => rfc9173::bcb_aes_gcm::parse(asb, data)
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
