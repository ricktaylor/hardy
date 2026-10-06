use alloc::boxed::Box;
use core::ptr::from_ref;

use hardy_cbor::encode::emit;
use smallvec::SmallVec;
use thiserror::Error;

#[cfg(feature = "rfc9173")]
use crate::bpsec::rfc9173;
use crate::{
    HashMap, HashSet, block,
    bpsec::{self, bib, key},
    builder, bundle, crc,
    editor::{self, Chunk, Editor},
    eid,
    reader::Reader,
};
/// Errors that can occur during bundle signing.
#[derive(Debug, Error)]
pub enum Error {
    /// The target block number is not a valid signing target (e.g., it is a BPSec block).
    #[error("Invalid block target {0}, either BCB or BIB block")]
    InvalidTarget(u64),

    /// The target block already has integrity protection from another BIB,
    /// or is already queued for signing in this session.
    #[error("Block target {0} is already signed, or queued for signing")]
    AlreadySigned(u64),

    /// The target block is encrypted by a BCB; sign before encrypting.
    #[error("Block target {0} is already the target of a BCB")]
    EncryptedTarget(u64),

    /// Signing fragmented bundles is not supported (RFC 9172 Section 5).
    #[error("Bundle is a fragment")]
    FragmentedBundle,

    /// An error occurred while editing the bundle.
    #[error(transparent)]
    Editor(#[from] editor::Error),
}

impl From<bpsec::Error> for Error {
    fn from(e: bpsec::Error) -> Self {
        Error::Editor(editor::Error::Builder(builder::Error::InternalError(
            e.into(),
        )))
    }
}

/// Integrity security context to apply when creating a BIB.
#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Hash, Eq, PartialEq)]
pub enum Context {
    /// BIB-HMAC-SHA2 context with the specified IPPT scope flags (RFC 9173 Section 3).
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2(rfc9173::ScopeFlags),

    /// Placeholder for future context types
    #[doc(hidden)]
    __Reserved,
}

impl Context {
    /// Whether operations under this context can share one BIB. Each
    /// security context decides: BIB-HMAC-SHA2 shares only under a scope
    /// that leaves the security header out, since a result bound to its
    /// BIB's block number breaks when an RFC 9172 §3.9 split at a waypoint
    /// moves it into a new BIB (RFC 9172 erratum 8723).
    pub fn can_share(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(scope_flags) => rfc9173::bib_hmac_sha2::can_share(scope_flags),
            Self::__Reserved => false,
        }
    }
}

struct BlockTemplate<'a> {
    context: Context,
    source: eid::Eid,
    key: &'a key::Key,
}

/// Accumulates BIB signing operations and rebuilds the bundle with integrity blocks.
pub struct Signer<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    templates: HashMap<u64, BlockTemplate<'a>>,
}

impl<'a> Signer<'a> {
    /// Creates a new signer for the given parsed bundle and its raw wire bytes.
    pub fn new(original: &'a bundle::Bundle, source_data: &'a [u8]) -> Self {
        Self {
            original,
            source_data,
            templates: HashMap::new(),
        }
    }

    /// Sign a block in the bundle.
    ///
    /// Targets queued with the same source, context and key reference (the
    /// same `&Key`, not merely an equal one) share one BIB when the context
    /// lets them (see [`Context::can_share`]); otherwise each gets its own.
    ///
    /// # Errors
    ///
    /// Besides the target checks, signing a primary block that carries a CRC
    /// removes it (RFC 9173 §3.8.1), changing the primary's bytes, so it is
    /// refused with
    /// [`PrimaryInSecurityScope`](editor::Error::PrimaryInSecurityScope)
    /// while an operation already in the bundle has the primary in its
    /// scope. On error, returns the signer along with the error so it can
    /// be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn sign_block(
        mut self,
        block_number: u64,
        context: Context,
        source: eid::Eid,
        key: &'a key::Key,
    ) -> Result<Self, (Self, Error)> {
        if self.original.primary.flags.is_fragment {
            return Err((self, Error::FragmentedBundle));
        }

        let Some(block) = self.original.blocks.get(&block_number) else {
            return Err((self, editor::Error::NoSuchBlock(block_number).into()));
        };

        if let block::Type::BlockIntegrity | block::Type::BlockSecurity = block.block_type {
            return Err((self, Error::InvalidTarget(block_number)));
        }

        match block.bib {
            block::BibCoverage::Some(_) => {
                return Err((self, Error::AlreadySigned(block_number)));
            }
            block::BibCoverage::Maybe => {
                return Err((self, bpsec::Error::MaybeHasBib(block_number).into()));
            }
            block::BibCoverage::None => {}
        }

        if block.bcb.is_some() {
            return Err((self, Error::EncryptedTarget(block_number)));
        }

        if self.templates.contains_key(&block_number) {
            return Err((self, Error::AlreadySigned(block_number)));
        }

        if block_number == 0
            && !matches!(self.original.primary.crc_type, crc::CrcType::None)
            && let Some(security_block) = Editor::new(self.original, self.source_data)
                .primary_scoped_operation(&HashSet::new())
        {
            return Err((
                self,
                editor::Error::PrimaryInSecurityScope(security_block).into(),
            ));
        }

        self.templates.insert(
            block_number,
            BlockTemplate {
                context,
                source,
                key,
            },
        );
        Ok(self)
    }

    /// Applies all queued signing operations and rebuilds the bundle as raw bytes.
    pub fn rebuild(self) -> Result<Box<[u8]>, Error> {
        let source_data = self.source_data;
        self.rebuild_editor()?
            .rebuild()
            .map(|c| Chunk::flatten(c, source_data))
            .map_err(Error::from)
    }

    /// Applies all queued signing operations and rebuilds the bundle,
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
            // No signing to do
            return Ok(Editor::new(self.original, self.source_data));
        }

        // Group the targets into BIBs. Where the context lets operations
        // share a block, targets share one per source, context and key: a BIB
        // carries one parameter set, so one key (RFC 9173 §3.8.2), and the
        // key stands for the security acceptors that can process the
        // operations, which must not differ within a block (RFC 9172 §3.3).
        // Otherwise each target gets its own BIB.
        type GroupKey = (eid::Eid, Context, *const key::Key, Option<u64>);
        type Group<'b> = (&'b key::Key, SmallVec<[u64; 4]>);
        let mut groups = HashMap::<GroupKey, Group<'a>>::new();
        for (block_number, template) in self.templates {
            let alone = (!template.context.can_share()).then_some(block_number);
            groups
                .entry((
                    template.source,
                    template.context,
                    from_ref(template.key),
                    alone,
                ))
                .or_insert_with(|| (template.key, SmallVec::new()))
                .1
                .push(block_number);
        }

        let mut editor = Editor::new(self.original, self.source_data);

        /* RFC 9173, Section 3.8.1 states:
         * Prior to the generation of the IPPT, if a Cyclic Redundancy Check
         * (CRC) value is present for the target block of the BIB, then that
         * CRC value MUST be removed from the target block.  This involves
         * both removing the CRC value from the target block and setting the
         * CRC type field of the target block to "no CRC is present."
         *
         * Every group's CRCs go before any group's IPPT is computed: a
         * primary-block target's CRC is part of every other target's IPPT
         * whose scope includes the primary. */
        for target in groups.values().flat_map(|(_, targets)| targets) {
            let target_block = self
                .original
                .blocks
                .get(target)
                .expect("Missing target block");
            if !matches!(target_block.crc_type, crc::CrcType::None) {
                // The rule applies to every target, including the primary
                // (RFC 9171 permits the primary to carry no CRC when a BIB
                // targets it).
                if *target == 0 {
                    // Re-emit the primary canonically with no CRC and
                    // register the bytes, so the IPPT (which reads the
                    // primary via `block(0)`) and the rebuilt bundle agree
                    // on the CRC-removed form.
                    let mut primary = self.original.primary.clone();
                    primary.crc_type = crc::CrcType::None;
                    let canonical = primary.emit().map_err(editor::Error::from)?;
                    editor.set_canonical_primary(primary, canonical.into());
                } else {
                    editor = editor
                        .update_block_inner(*target)
                        .map_err(|(_, e)| e)?
                        .with_crc_type(crc::CrcType::None)
                        .rebuild();
                }
            }
        }

        // Now build BIB blocks
        for ((bpsec_source, context, _, _), (key, targets)) in groups {
            // Reserve a block number for the BIB block
            let b = editor
                .alloc_block(block::Type::BlockIntegrity)
                .map_err(|(_, e)| e)?
                .with_crc_type(crc::CrcType::None);

            let source = b.block_number();
            editor = b.rebuild();

            let editor_bs = editor::EditorReader { editor };

            let operation_set = bib::OperationSet {
                source: bpsec_source.clone(),
                operations: sign_targets(
                    &context,
                    key,
                    &bpsec_source,
                    &targets,
                    source,
                    &editor_bs,
                )?,
            };

            // Rewrite with the real data
            editor = editor_bs
                .editor
                .update_block_inner(source)
                .map_err(|(_, e)| e)?
                .with_data(emit(&operation_set).0.into())
                .rebuild();

            // Set BIB coverage on target blocks
            for target in operation_set.operations.keys() {
                editor.set_bib_target(*target, source);
            }
        }

        Ok(editor)
    }
}

// Signs every target of one BIB, all under one keying.
#[allow(unused_variables)]
fn sign_targets<'b>(
    context: &Context,
    key: &key::Key,
    bpsec_source: &'b eid::Eid,
    targets: &[u64],
    source: u64,
    blocks: &'b dyn Reader<'b>,
) -> Result<HashMap<u64, bib::Operation>, bpsec::Error> {
    #[cfg(feature = "rfc9173")]
    if let Context::HMAC_SHA2(scope_flags) = context {
        let keying = rfc9173::bib_hmac_sha2::Keying::new(key)?;
        return targets
            .iter()
            .map(|&target| {
                let operation = rfc9173::bib_hmac_sha2::Operation::sign(
                    &keying,
                    scope_flags.clone(),
                    bib::OperationArgs {
                        bpsec_source,
                        target,
                        source,
                        blocks,
                    },
                )?;
                Ok((target, bib::Operation::HMAC_SHA2(operation)))
            })
            .collect();
    }

    // Reachable when no security context feature is enabled (e.g.
    // `--no-default-features` with no `rfc9173`), or when a caller
    // constructs `Context::__Reserved`. Returns a typed error rather
    // than panicking, so an unsupported context is a signature-level
    // failure.
    Err(bpsec::Error::UnsupportedOperation)
}
