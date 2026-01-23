use super::*;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("No such block number {0}")]
    NoSuchBlock(u64),

    #[error("Invalid block target {0}, either BCB or BIB block")]
    InvalidTarget(u64),

    #[error("Block target {0} is already signed with another BIB")]
    AlreadySigned(u64),

    #[error("Block target {0} is already the target of a BCB")]
    EncryptedTarget(u64),

    #[error(transparent)]
    Editor(#[from] editor::Error),

    #[error(transparent)]
    Security(#[from] bpsec::Error),
}

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Hash, Eq, PartialEq)]
pub enum Context {
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2(rfc9173::ScopeFlags),
}

struct BlockTemplate<'a> {
    context: Context,
    source: eid::Eid,
    key: &'a key::Key,
}

pub struct Signer<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    templates: HashMap<u64, BlockTemplate<'a>>,
}

impl<'a> Signer<'a> {
    pub fn new(original: &'a bundle::Bundle, source_data: &'a [u8]) -> Self {
        Self {
            original,
            source_data,
            templates: HashMap::new(),
        }
    }

    /// Sign a block in the bundle.
    ///
    /// On error, returns the signer along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn sign_block(
        mut self,
        block_number: u64,
        context: Context,
        source: eid::Eid,
        key: &'a key::Key,
    ) -> Result<Self, (Self, Error)> {
        let block = match self.original.blocks.get(&block_number) {
            Some(b) => b,
            None => return Err((self, Error::NoSuchBlock(block_number))),
        };

        if let block::Type::BlockIntegrity | block::Type::BlockSecurity = block.block_type {
            return Err((self, Error::InvalidTarget(block_number)));
        }

        if block.bib.is_some() {
            return Err((self, Error::AlreadySigned(block_number)));
        }

        if block.bcb.is_some() {
            return Err((self, Error::EncryptedTarget(block_number)));
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

    pub fn rebuild(self) -> Result<Box<[u8]>, Error> {
        if self.templates.is_empty() {
            // No signing to do
            return editor::Editor::new(self.original, self.source_data)
                .rebuild()
                .map_err(Into::into);
        }

        // Reorder and accumulate BIB operations
        let mut blocks = HashMap::<(eid::Eid, Context), Vec<(u64, &'a key::Key)>>::new();
        for (block_number, template) in self.templates {
            blocks
                .entry((template.source, template.context))
                .or_default()
                .push((block_number, template.key));
        }

        let mut editor = editor::Editor::new(self.original, self.source_data);

        // Now build BIB blocks
        for ((bpsec_source, context), targets) in blocks {
            /* RFC 9173, Section 3.8.1 states:
             * Prior to the generation of the IPPT, if a Cyclic Redundancy Check
             * (CRC) value is present for the target block of the BIB, then that
             * CRC value MUST be removed from the target block.  This involves
             * both removing the CRC value from the target block and setting the
             * CRC type field of the target block to "no CRC is present." */
            for (target, _) in &targets {
                let target_block = self
                    .original
                    .blocks
                    .get(target)
                    .expect("Missing target block");
                if *target != 0 && !matches!(target_block.crc_type, crc::CrcType::None) {
                    editor = editor
                        .update_block(*target)
                        .map_err(|(_, e)| e)?
                        .with_crc_type(crc::CrcType::None)
                        .rebuild();
                }
            }

            // Reserve a block number for the BIB block
            let b = editor
                .push_block(block::Type::BlockIntegrity)
                .map_err(|(_, e)| e)?
                .with_crc_type(crc::CrcType::None);

            let source = b.block_number();
            editor = b.rebuild();

            let editor_bs = editor::EditorBlockSet { editor };

            let mut operation_set = bib::OperationSet {
                source: bpsec_source.clone(),
                operations: HashMap::new(),
            };

            for (target, key) in targets {
                operation_set.operations.insert(
                    target,
                    build_bib_data(
                        context.clone(),
                        bib::OperationArgs {
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
                .update_block(source)
                .map_err(|(_, e)| e)?
                .with_data(hardy_cbor::encode::emit(&operation_set).0.into())
                .rebuild();
        }

        editor.rebuild().map_err(Into::into)
    }
}

#[allow(irrefutable_let_patterns)]
fn build_bib_data(
    context: Context,
    args: bib::OperationArgs,
    key: &key::Key,
) -> Result<bib::Operation, bpsec::Error> {
    #[cfg(feature = "rfc9173")]
    if let Context::HMAC_SHA2(scope_flags) = context {
        return Ok(bib::Operation::HMAC_SHA2(
            rfc9173::bib_hmac_sha2::Operation::sign(key, scope_flags, args)?,
        ));
    }

    panic!("Unsupported BIB context!");
}
