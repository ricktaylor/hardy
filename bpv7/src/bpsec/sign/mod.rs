use alloc::boxed::Box;

use hardy_cbor::encode::Encoder;

use crate::HashMap;
use crate::bpsec::asb::{AbstractSecurityBlock, OperationArgs};
use crate::bpsec::error::Error;
use crate::bpsec::{self, Context, key};
use crate::crc;
use crate::editor::{self, Chunk, Editor};
use crate::{block, bundle, eid};
use smallvec::SmallVec;

pub use crate::bpsec::asb::UnknownOperation;

#[cfg(feature = "rfc9173")]
pub mod hmac_sha2;

/// A parsed BIB (Block Integrity Block) security context with operation data.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum SignContext {
    /// HMAC-SHA2 integrity operation (RFC 9173).
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2(hmac_sha2::Operation),
    /// An unrecognised security context (context ID, raw parameters/results).
    Unrecognised(u64, UnknownOperation),
}

impl SignContext {
    pub fn is_unsupported(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(operation) => operation.is_unsupported(),
            Self::Unrecognised(..) => true,
        }
    }

    /// Verifies the integrity of the target block.
    #[allow(unused_variables)]
    pub fn verify<K>(&self, key_source: &K, args: OperationArgs) -> Result<(), Error>
    where
        K: key::KeySource + ?Sized,
    {
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

    /// Encode the security context ID, flags, source, and parameters.
    pub fn emit_context(&self, encoder: &mut Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    /// Encode the per-target result.
    pub fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array),
        }
    }
}

/// Parse a BIB Abstract Security Block from raw CBOR data.
///
/// Returns the security source EID, a map of target block number to `SignContext`,
/// a boolean indicating whether the encoding was shortest-form, and the number
/// of bytes consumed.
pub fn parse_asb(data: &[u8]) -> Result<(eid::Eid, HashMap<u64, SignContext>, bool, usize), Error> {
    let (asb, shortest, len) =
        hardy_cbor::decode::parse::<(AbstractSecurityBlock, bool, usize)>(data)?;

    #[allow(unreachable_patterns)]
    match asb.context {
        #[cfg(feature = "rfc9173")]
        Context::BIB_HMAC_SHA2 => hmac_sha2::parse(asb, data)
            .map(|(source, operations, s)| (source, operations, shortest && s, len)),
        Context::Unrecognised(id) => {
            UnknownOperation::parse(asb, data).map(|(source, operations)| {
                (
                    source,
                    operations
                        .into_iter()
                        .map(|(t, o)| (t, SignContext::Unrecognised(id, o)))
                        .collect(),
                    shortest,
                    len,
                )
            })
        }
        c => Err(Error::InvalidContext(c)),
    }
}

/// Encode a BIB operation set as an Abstract Security Block.
pub fn encode_asb(source: &eid::Eid, operations: &HashMap<u64, SignContext>) -> Box<[u8]> {
    let mut encoder = hardy_cbor::encode::Encoder::new();
    let (targets, ops): (SmallVec<[&u64; 4]>, SmallVec<[&SignContext; 4]>) =
        operations.iter().unzip();

    encoder.emit(targets.as_slice());

    ops.first().unwrap().emit_context(&mut encoder, source);

    encoder.emit_array(Some(ops.len()), |a| {
        for op in ops {
            op.emit_result(a);
        }
    });

    encoder.build().into()
}

/// Check if any operations in the map are unsupported.
pub fn is_unsupported(operations: &HashMap<u64, SignContext>) -> bool {
    operations.values().any(|op| op.is_unsupported())
}

/// Sign target blocks in a bundle with BIB integrity protection using default context parameters.
///
/// The `targets` slice contains tuples of `(block_number, source_eid, key)`.
/// Uses default scope flags (all included). For custom flags, use [`sign_with`].
pub fn sign(
    bundle: &bundle::Bundle,
    data: &[u8],
    targets: &[(u64, eid::Eid, &key::Key)],
) -> Result<Box<[u8]>, Error> {
    let with_defaults: SmallVec<[(u64, hmac_sha2::ScopeFlags, eid::Eid, &key::Key); 4]> = targets
        .iter()
        .map(|(bn, src, key)| (*bn, hmac_sha2::ScopeFlags::default(), src.clone(), *key))
        .collect();
    sign_with(bundle, data, &with_defaults)
}

/// Sign target blocks in a bundle with BIB integrity protection and explicit context parameters.
///
/// The `targets` slice contains tuples of `(block_number, scope_flags, source_eid, key)`.
/// Returns the rebuilt bundle as raw bytes.
pub fn sign_with<'a>(
    bundle: &'a bundle::Bundle,
    data: &'a [u8],
    targets: &[(u64, hmac_sha2::ScopeFlags, eid::Eid, &key::Key)],
) -> Result<Box<[u8]>, Error> {
    sign_editor(bundle, data, targets)?
        .rebuild()
        .map(|c| Chunk::flatten(c, data))
        .map_err(Error::from)
}

fn sign_editor<'a>(
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    targets: &[(u64, hmac_sha2::ScopeFlags, eid::Eid, &'a key::Key)],
) -> Result<Editor<'a>, Error> {
    if targets.is_empty() {
        return Ok(Editor::new(original, source_data));
    }

    // Validate all targets first
    if original.flags.is_fragment {
        return Err(Error::FragmentedBundle);
    }

    for (block_number, _, _, _) in targets {
        let block = original
            .blocks
            .get(block_number)
            .ok_or(editor::Error::NoSuchBlock(*block_number))?;

        if let block::Type::BlockIntegrity | block::Type::BlockSecurity = block.block_type {
            return Err(Error::InvalidSignTarget(*block_number));
        }

        match block.bib {
            block::BibCoverage::Some(_) => {
                return Err(Error::AlreadySigned(*block_number));
            }
            block::BibCoverage::Maybe => {
                return Err(bpsec::Error::MaybeHasBib(*block_number));
            }
            block::BibCoverage::None => {}
        }

        if block.bcb.is_some() {
            return Err(Error::EncryptedTarget(*block_number));
        }
    }

    // Reorder and accumulate BIB operations
    type TargetVec<'b> = SmallVec<[(u64, &'b key::Key); 4]>;
    let mut blocks = HashMap::<(eid::Eid, hmac_sha2::ScopeFlags), TargetVec<'a>>::new();
    for (block_number, context, source, key) in targets {
        blocks
            .entry((source.clone(), context.clone()))
            .or_default()
            .push((*block_number, *key));
    }

    let mut editor = Editor::new(original, source_data);

    // Now build BIB blocks
    for ((bpsec_source, context), bib_targets) in blocks {
        /* RFC 9173, Section 3.8.1 states:
         * Prior to the generation of the IPPT, if a Cyclic Redundancy Check
         * (CRC) value is present for the target block of the BIB, then that
         * CRC value MUST be removed from the target block.  This involves
         * both removing the CRC value from the target block and setting the
         * CRC type field of the target block to "no CRC is present." */
        for (target, _) in &bib_targets {
            let target_block = original.blocks.get(target).expect("Missing target block");
            if *target != 0 && !matches!(target_block.crc_type, crc::CrcType::None) {
                editor = editor
                    .update_block_inner(*target)
                    .map_err(|(_, e)| e)?
                    .with_crc_type(crc::CrcType::None)
                    .rebuild();
            }
        }

        // Reserve a block number for the BIB block
        let b = editor
            .alloc_block(block::Type::BlockIntegrity)
            .map_err(|(_, e)| e)?
            .with_crc_type(crc::CrcType::None);

        let source = b.block_number();
        editor = b.rebuild();

        let editor_bs = editor::EditorBlockSet { editor };

        let mut operations = HashMap::with_capacity(bib_targets.len());

        for (target, key) in bib_targets {
            operations.insert(
                target,
                build_bib_data(
                    context.clone(),
                    OperationArgs {
                        bpsec_source: &bpsec_source,
                        target,
                        source,
                        blocks: &editor_bs,
                    },
                    key,
                )?,
            );
        }

        // Rewrite with the real data
        editor = editor_bs
            .editor
            .update_block_inner(source)
            .map_err(|(_, e)| e)?
            .with_data(encode_asb(&bpsec_source, &operations).into_vec().into())
            .rebuild();

        // Set BIB coverage on target blocks
        for target in operations.keys() {
            editor.set_bib_target(*target, source);
        }
    }

    Ok(editor)
}

#[cfg(feature = "rfc9173")]
fn build_bib_data(
    scope_flags: hmac_sha2::ScopeFlags,
    args: OperationArgs,
    key: &key::Key,
) -> Result<SignContext, bpsec::Error> {
    Ok(SignContext::HMAC_SHA2(hmac_sha2::Operation::sign(
        key,
        scope_flags,
        args,
    )?))
}
