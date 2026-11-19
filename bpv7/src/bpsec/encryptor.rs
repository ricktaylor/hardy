use alloc::boxed::Box;
use core::ptr::from_ref;

use hardy_cbor::encode::emit;
use smallvec::SmallVec;
use thiserror::Error;

#[cfg(feature = "rfc9173")]
use crate::bpsec::rfc9173;
use crate::{
    HashMap, block,
    bpsec::{self, bcb, key},
    bundle, crc,
    editor::{self, Chunk, Editor},
    eid,
};
/// Errors that can occur during bundle encryption.
#[derive(Debug, Error)]
pub enum Error {
    /// The target block number is not a valid encryption target (e.g., it is a BPSec block).
    #[error("Invalid block target {0}, BCB block")]
    InvalidTarget(u64),

    /// The target block is already encrypted by another BCB.
    #[error("Block target {0} is already the target of a BCB")]
    AlreadyEncrypted(u64),

    /// The BIB over the target, named here, also signs the primary block.
    /// Encrypting the target encrypts that BIB and every block it covers
    /// (RFC 9172 §3.9), and a BCB cannot target the primary block.
    #[error("BIB {0} over the target also covers the primary block, which a BCB cannot target")]
    BibCoversPrimary(u64),

    /// Encryption of fragmented bundles is not supported (RFC 9172 Section 5).
    #[error("Bundle is a fragment")]
    FragmentedBundle,

    /// An error occurred while editing the bundle.
    #[error(transparent)]
    Editor(#[from] editor::Error),
}

impl From<crate::Error> for Error {
    fn from(e: crate::Error) -> Self {
        Error::Editor(e.into())
    }
}

impl From<bpsec::Error> for Error {
    fn from(e: bpsec::Error) -> Self {
        Error::Editor(e.into())
    }
}

/// Encryption security context to apply when creating a BCB.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Hash, Eq, PartialEq)]
pub enum Context {
    /// BCB-AES-GCM context with the specified IPPT scope flags (RFC 9173 Section 4).
    #[cfg(feature = "rfc9173")]
    AES_GCM(rfc9173::ScopeFlags),

    /// Placeholder for future context types
    #[doc(hidden)]
    __Reserved,
}

struct BlockTemplate<'a> {
    context: Context,
    source: eid::Eid,
    key: &'a key::Key,
}

/// Accumulates BCB encryption operations and rebuilds the bundle with encrypted blocks.
pub struct Encryptor<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    templates: HashMap<u64, BlockTemplate<'a>>,
}

impl<'a> Encryptor<'a> {
    /// Creates a new encryptor for the given parsed bundle and its raw wire bytes.
    pub fn new(original: &'a bundle::Bundle, source_data: &'a [u8]) -> Self {
        Self {
            original,
            source_data,
            templates: HashMap::new(),
        }
    }

    /// Encrypt a block in the bundle.
    ///
    /// A BIB over the target is encrypted with it, as RFC 9172 §3.9
    /// requires, together with every other block that BIB covers, each under
    /// its own BCB. The BIB is never split: moved results survive only where
    /// its scope leaves out the security header, and would then have to keep
    /// their original source, which RFC 9172 §3.6 does not let the new BIB
    /// name.
    ///
    /// BCB-AES-GCM gives every target its own BCB, for its own IV. Under a
    /// context that lets targets share a BCB, targets queued with the same
    /// source, context and key reference (the same `&Key`, not merely an
    /// equal one) share one.
    ///
    /// # Errors
    ///
    /// Among the target checks, [`Error::BibCoversPrimary`] when the BIB
    /// over the target also covers the primary block, which no BCB can
    /// target.
    pub fn encrypt_block(
        &mut self,
        block_number: u64,
        context: Context,
        source: eid::Eid,
        key: &'a key::Key,
    ) -> Result<(), Error> {
        if self.original.primary.flags.is_fragment {
            return Err(Error::FragmentedBundle);
        }

        if block_number == 0 {
            return Err(Error::InvalidTarget(block_number));
        }

        let Some(block) = self.original.blocks.get(&block_number) else {
            return Err(editor::Error::NoSuchBlock(block_number).into());
        };

        if block.bcb.is_some() {
            return Err(Error::AlreadyEncrypted(block_number));
        }

        if let block::Type::BlockIntegrity | block::Type::BlockSecurity = block.block_type {
            return Err(Error::InvalidTarget(block_number));
        }

        // RFC 9172 §3.9: a BCB over a block a BIB covers must encrypt the
        // BIB too. Every other target of that BIB is encrypted with it rather
        // than the BIB split: the moved results would keep their original
        // source, which §3.6 does not let the new BIB name.
        match block.bib {
            block::BibCoverage::Maybe => {
                return Err(editor::Error::MaybeHasBib(block_number).into());
            }
            block::BibCoverage::Some(bib_block) => {
                let Some(bib) = self.original.blocks.get(&bib_block) else {
                    return Err(crate::Error::Altered.into());
                };

                // `source_data` is the full in-memory bundle being edited.
                let opset = match bib.extract::<bpsec::bib::OperationSet>(self.source_data) {
                    Ok(Some(opset)) => opset,
                    Ok(None) => {
                        return Err(crate::Error::Altered.into());
                    }
                    Err(e) => {
                        return Err(crate::Error::InvalidField {
                            field: "BIB Abstract Syntax Block",
                            source: e.into(),
                        }
                        .into());
                    }
                };

                if opset.operations.contains_key(&0) {
                    return Err(Error::BibCoversPrimary(bib_block));
                }

                // Encrypt all the BIB targets
                for target in opset.operations.keys() {
                    if *target != block_number {
                        self.templates.insert(
                            *target,
                            BlockTemplate {
                                context: context.clone(),
                                source: source.clone(),
                                key,
                            },
                        );
                    }
                }

                // Encrypt the BIB itself
                self.templates.insert(
                    bib_block,
                    BlockTemplate {
                        context: context.clone(),
                        source: source.clone(),
                        key,
                    },
                );
            }
            block::BibCoverage::None => {}
        }

        self.templates.insert(
            block_number,
            BlockTemplate {
                context,
                source,
                key,
            },
        );
        Ok(())
    }

    /// Applies all queued encryption operations and rebuilds the bundle as raw bytes.
    pub fn rebuild(self) -> Result<Box<[u8]>, Error> {
        let source_data = self.source_data;
        self.rebuild_editor()?
            .rebuild()
            .map(|c| Chunk::flatten(c, source_data))
            .map_err(Error::from)
    }

    /// Applies all queued encryption operations and rebuilds the bundle,
    /// returning both the updated `Bundle` and the serialized data.
    pub fn rebuild_bundle(self) -> Result<(bundle::Bundle, Box<[u8]>), Error> {
        let source_data = self.source_data;
        self.rebuild_editor()?
            .rebuild_bundle()
            .map(|(b, c)| (b, Chunk::flatten(c, source_data)))
            .map_err(Error::from)
    }

    fn rebuild_editor(self) -> Result<Editor<'a>, Error> {
        if self.templates.is_empty() {
            return Ok(Editor::new(self.original, self.source_data));
        }

        // Reorder and accumulate BCB operations if sharing is possible
        type TargetVec<'b> = SmallVec<[(u64, &'b key::Key); 4]>;
        let mut bcbs: SmallVec<[(eid::Eid, Context, TargetVec<'a>); 4]> = SmallVec::new();
        // Shared BCBs group per source, context and key: the key stands for
        // the security acceptors, which must not differ within a block (RFC
        // 9172 §3.3).
        let mut shared_bcbs = HashMap::<(eid::Eid, Context, *const key::Key), TargetVec<'a>>::new();
        for (block_number, template) in self.templates {
            // Check if this context supports sharing multiple targets in one BCB.
            // AES_GCM requires unique IVs per target, so each target gets its own BCB.
            // Future contexts (e.g., COSE-based) may allow sharing if they store
            // per-target IVs in results rather than shared parameters.
            //
            // TODO: When adding new contexts, update this match to check each context's
            // sharing capability. Consider calling a can_share() method on the built
            // operation if sharing depends on operation parameters.
            match &template.context {
                #[cfg(feature = "rfc9173")]
                Context::AES_GCM(_) => {
                    bcbs.push((
                        template.source,
                        template.context,
                        smallvec::smallvec![(block_number, template.key)],
                    ));
                }
                _ => {
                    shared_bcbs
                        .entry((template.source, template.context, from_ref(template.key)))
                        .or_default()
                        .push((block_number, template.key));
                }
            }
        }

        // Add all shared BCBs to the total BCBs
        bcbs.extend(
            shared_bcbs
                .into_iter()
                .map(|((bpsec_source, context, _), targets)| (bpsec_source, context, targets)),
        );

        let mut editor = Editor::new(self.original, self.source_data);

        // Now build BCB blocks. Every target keeps its CRC type as it
        // stands, the CRC computed over the ciphertext as the block is
        // emitted: no security operation changes a target's CRC, and the
        // CRC is no part of the additional authenticated data (RFC 9173
        // §4.7).
        for (bpsec_source, context, targets) in bcbs {
            // Reserve a block number for the BCB block
            let b = editor
                .alloc_block(block::Type::BlockSecurity)?
                .with_crc_type(crc::CrcType::None)
                .with_flags(block::Flags {
                    must_replicate: true,
                    ..Default::default()
                });

            let security_block = b.block_number();
            b.rebuild();

            let mut operations = HashMap::with_capacity(targets.len());
            for (target, key) in targets {
                let (op, data) = build_bcb_data(
                    context.clone(),
                    bcb::OperationArgs {
                        source: &bpsec_source,
                        target,
                        security_block,
                        blocks: &editor,
                    },
                    key,
                )?;

                // Rewrite the target block
                editor
                    .update_block_inner(target)?
                    .with_data(data.into_vec().into())
                    .rebuild();

                operations.insert(target, op);
            }

            // Set BCB coverage on target blocks before operations is moved
            let target_blocks: SmallVec<[u64; 4]> = operations.keys().copied().collect();

            // Rewrite the BCB with the real data
            editor
                .update_block_inner(security_block)?
                .with_data(
                    emit(&bcb::OperationSet {
                        source: bpsec_source,
                        operations,
                    })
                    .0
                    .into(),
                )
                .rebuild();

            for target in target_blocks {
                editor.set_bcb_target(target, security_block);
            }
        }

        Ok(editor)
    }
}

#[allow(unused_variables)]
fn build_bcb_data(
    context: Context,
    args: bcb::OperationArgs,
    key: &key::Key,
) -> Result<(bcb::Operation, Box<[u8]>), bpsec::Error> {
    #[cfg(feature = "rfc9173")]
    if let Context::AES_GCM(scope_flags) = context {
        let (op, data) = rfc9173::bcb_aes_gcm::Operation::encrypt(key, scope_flags, args)?;
        return Ok((bcb::Operation::AES_GCM(op), data));
    }

    // Reachable when no security context feature is enabled (e.g.
    // `--no-default-features` with no `rfc9173`), or when a caller
    // constructs `Context::__Reserved`. Returns a typed error rather
    // than panicking, so an unsupported context is a signature-level
    // failure.
    let _ = (context, args, key);
    Err(bpsec::Error::UnsupportedOperation)
}
