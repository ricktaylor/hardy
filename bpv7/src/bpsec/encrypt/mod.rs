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
pub mod aes_gcm;

/// A parsed BCB (Block Confidentiality Block) security context with operation data.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum EncryptContext {
    /// AES-GCM encryption operation (RFC 9173).
    #[cfg(feature = "rfc9173")]
    AES_GCM(aes_gcm::Operation),
    /// An unrecognised security context (context ID, raw parameters/results).
    Unrecognised(u64, UnknownOperation),
}

impl EncryptContext {
    pub fn is_unsupported(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(operation) => operation.is_unsupported(),
            Self::Unrecognised(..) => true,
        }
    }

    pub fn can_share(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(_) => false,
            Self::Unrecognised(..) => false,
        }
    }

    /// Decrypts the target block.
    #[allow(unused_variables)]
    pub fn decrypt<K>(
        &self,
        key_source: &K,
        args: OperationArgs,
    ) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error>
    where
        K: key::KeySource + ?Sized,
    {
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

    /// Encode the security context ID, flags, source, and parameters.
    pub fn emit_context(&self, encoder: &mut Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    /// Encode the per-target result.
    pub fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array),
        }
    }
}

/// Parse a BCB Abstract Security Block from raw CBOR data.
///
/// Returns the security source EID, a map of target block number to `EncryptContext`,
/// a boolean indicating whether the encoding was shortest-form, and the number
/// of bytes consumed.
pub fn parse_asb(
    data: &[u8],
) -> Result<(eid::Eid, HashMap<u64, EncryptContext>, bool, usize), Error> {
    let (asb, shortest, len) =
        hardy_cbor::decode::parse::<(AbstractSecurityBlock, bool, usize)>(data)?;

    #[allow(unreachable_patterns)]
    match asb.context {
        #[cfg(feature = "rfc9173")]
        Context::BCB_AES_GCM => aes_gcm::parse(asb, data)
            .map(|(source, operations, s)| (source, operations, shortest && s, len)),
        Context::Unrecognised(id) => {
            UnknownOperation::parse(asb, data).map(|(source, operations)| {
                (
                    source,
                    operations
                        .into_iter()
                        .map(|(t, o)| (t, EncryptContext::Unrecognised(id, o)))
                        .collect(),
                    shortest,
                    len,
                )
            })
        }
        c => Err(Error::InvalidContext(c)),
    }
}

/// Encode a BCB operation set as an Abstract Security Block.
pub fn encode_asb(source: &eid::Eid, operations: &HashMap<u64, EncryptContext>) -> Box<[u8]> {
    let mut encoder = hardy_cbor::encode::Encoder::new();
    let (targets, ops): (SmallVec<[&u64; 4]>, SmallVec<[&EncryptContext; 4]>) =
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
pub fn is_unsupported(operations: &HashMap<u64, EncryptContext>) -> bool {
    operations.values().any(|op| op.is_unsupported())
}

/// Check if the operations support sharing targets in a single BCB.
pub fn can_share(operations: &HashMap<u64, EncryptContext>) -> bool {
    operations.values().next().is_some_and(|op| op.can_share())
}

/// Encrypt target blocks in a bundle with BCB confidentiality protection using default context parameters.
///
/// The `targets` slice contains tuples of `(block_number, source_eid, key)`.
/// Uses default scope flags (all included). For custom flags, use [`encrypt_with`].
pub fn encrypt(
    bundle: &bundle::Bundle,
    data: &[u8],
    targets: &[(u64, eid::Eid, &key::Key)],
) -> Result<Box<[u8]>, Error> {
    let with_defaults: SmallVec<[(u64, aes_gcm::ScopeFlags, eid::Eid, &key::Key); 4]> = targets
        .iter()
        .map(|(bn, src, key)| (*bn, aes_gcm::ScopeFlags::default(), src.clone(), *key))
        .collect();
    encrypt_with(bundle, data, &with_defaults)
}

/// Encrypt target blocks in a bundle with BCB confidentiality protection and explicit context parameters.
///
/// The `targets` slice contains tuples of `(block_number, scope_flags, source_eid, key)`.
/// Returns the rebuilt bundle as raw bytes.
pub fn encrypt_with<'a>(
    bundle: &'a bundle::Bundle,
    data: &'a [u8],
    targets: &[(u64, aes_gcm::ScopeFlags, eid::Eid, &key::Key)],
) -> Result<Box<[u8]>, Error> {
    let source_data = data;
    encrypt_editor(bundle, data, targets)?
        .rebuild()
        .map(|c| Chunk::flatten(c, source_data))
        .map_err(Error::from)
}

fn encrypt_editor<'a>(
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    targets: &[(u64, aes_gcm::ScopeFlags, eid::Eid, &'a key::Key)],
) -> Result<Editor<'a>, Error> {
    if targets.is_empty() {
        return Ok(Editor::new(original, source_data));
    }

    if original.flags.is_fragment {
        return Err(Error::FragmentedBundle);
    }

    // Build expanded targets (including BIB targets per RFC 9172 Section 3.9)
    let mut templates: HashMap<u64, (aes_gcm::ScopeFlags, eid::Eid, &'a key::Key)> = HashMap::new();

    for (block_number, context, source, key) in targets {
        if *block_number == 0 {
            return Err(Error::InvalidEncryptTarget(*block_number));
        }

        let block = original
            .blocks
            .get(block_number)
            .ok_or(editor::Error::NoSuchBlock(*block_number))?;

        if block.bcb.is_some() {
            return Err(Error::AlreadyEncrypted(*block_number));
        }

        if let block::Type::BlockIntegrity | block::Type::BlockSecurity = block.block_type {
            return Err(Error::InvalidEncryptTarget(*block_number));
        }

        /* RFC 9172 Section 3.9 states that BCBs targetting blocks with BIBs MUST also target the BIB
         * We take the 'all-or-nothing' approach and encrypt all BIB targets, rather than splitting the BIB
         * because splitting requires integrity keys */
        match block.bib {
            block::BibCoverage::Maybe => {
                return Err(bpsec::Error::MaybeHasBib(*block_number));
            }
            block::BibCoverage::Some(bib_block) => {
                let bib = match original.blocks.get(&bib_block) {
                    Some(b) => b,
                    None => {
                        return Err(crate::Error::Altered.into());
                    }
                };

                let bib_payload = match bib.payload(source_data) {
                    Some(p) => p,
                    None => {
                        return Err(crate::Error::Altered.into());
                    }
                };

                let (_, bib_operations, _, _) = match parse_bib_asb(bib_payload) {
                    Ok(result) => result,
                    Err(e) => {
                        return Err(crate::Error::InvalidField {
                            field: "BIB Abstract Syntax Block",
                            source: e.into(),
                        }
                        .into());
                    }
                };

                // Encrypt all the BIB targets
                for target in bib_operations.keys() {
                    if *target != *block_number {
                        templates.insert(*target, (context.clone(), source.clone(), *key));
                    }
                }

                // Encrypt the BIB itself
                templates.insert(bib_block, (context.clone(), source.clone(), *key));
            }
            block::BibCoverage::None => {}
        }

        templates.insert(*block_number, (context.clone(), source.clone(), *key));
    }

    // AES-GCM requires unique IVs per target, so each target gets its own BCB.
    let mut bcbs: SmallVec<[(eid::Eid, aes_gcm::ScopeFlags, u64, &'a key::Key); 4]> =
        SmallVec::new();
    for (block_number, (context, source, key)) in templates {
        bcbs.push((source, context, block_number, key));
    }

    let mut editor = Editor::new(original, source_data);

    // Now build BCB blocks (one per target for AES-GCM IV uniqueness)
    for (bpsec_source, context, target, key) in bcbs {
        // Remove CRC from target (RFC 9173 Section 4.8.1)
        let target_block = original.blocks.get(&target).expect("Missing target block");
        if !matches!(target_block.crc_type, crc::CrcType::None) {
            editor = editor
                .update_block_inner(target)
                .map_err(|(_, e)| e)?
                .with_crc_type(crc::CrcType::None)
                .rebuild();
        }

        // Reserve a block number for the BCB block
        let b = editor
            .alloc_block(block::Type::BlockSecurity)
            .map_err(|(_, e)| e)?
            .with_crc_type(crc::CrcType::None)
            .with_flags(block::Flags {
                must_replicate: true,
                ..Default::default()
            });

        let source = b.block_number();
        editor = b.rebuild();

        let editor_bs = editor::EditorBlockSet { editor };
        let (op, encrypted_data) = build_bcb_data(
            context,
            OperationArgs {
                bpsec_source: &bpsec_source,
                target,
                source,
                blocks: &editor_bs,
            },
            key,
        )?;

        // Rewrite the target block with ciphertext
        let editor_after = editor_bs
            .editor
            .update_block_inner(target)
            .map_err(|(_, e)| e)?
            .with_data(encrypted_data.into_vec().into())
            .rebuild();

        // Rewrite the BCB with the ASB data
        let mut operations = HashMap::with_capacity(1);
        operations.insert(target, op);
        editor = editor_after
            .update_block_inner(source)
            .map_err(|(_, e)| e)?
            .with_data(encode_asb(&bpsec_source, &operations).into_vec().into())
            .rebuild();

        editor.set_bcb_target(target, source);
    }

    Ok(editor)
}

/// Parse a BIB ASB from data. Used internally when we need to inspect BIB targets
/// during encryption.
fn parse_bib_asb(
    data: &[u8],
) -> Result<
    (
        eid::Eid,
        HashMap<u64, crate::bpsec::sign::SignContext>,
        bool,
        usize,
    ),
    Error,
> {
    crate::bpsec::sign::parse_asb(data)
}

#[allow(unused_variables)]
#[cfg(feature = "rfc9173")]
fn build_bcb_data(
    scope_flags: aes_gcm::ScopeFlags,
    args: OperationArgs,
    key: &key::Key,
) -> Result<(EncryptContext, Box<[u8]>), bpsec::Error> {
    let (op, data) = aes_gcm::Operation::encrypt(key, scope_flags, args)?;
    Ok((EncryptContext::AES_GCM(op), data))
}
