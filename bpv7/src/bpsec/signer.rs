use super::*;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("No such block number {0}")]
    NoSuchBlock(u64),

    #[error("Invalid block target {0}, either BCB or BIB block")]
    InvalidTarget(u64),

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

struct BlockTemplate {
    context: Context,
    source: eid::Eid,
    key: key::Key,
}

pub struct Signer<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    templates: HashMap<u64, BlockTemplate>,
}

impl<'a> Signer<'a> {
    pub fn new(original: &'a bundle::Bundle, source_data: &'a [u8]) -> Self {
        Self {
            original,
            source_data,
            templates: HashMap::new(),
        }
    }

    pub fn sign_block(
        mut self,
        block_number: u64,
        context: Context,
        source: eid::Eid,
        key: key::Key,
    ) -> Result<Self, Error> {
        let Some(block) = self.original.blocks.get(&block_number) else {
            return Err(Error::NoSuchBlock(block_number));
        };

        if let block::Type::BlockIntegrity | block::Type::BlockSecurity = block.block_type {
            return Err(Error::InvalidTarget(block_number));
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

    /// Create an `Encryptor` to encrypt blocks in the bundle.
    ///
    /// Note that this consumes the `Siner`, so any modifications made to the
    /// bundle prior to calling this method will be completed prior to signing.
    pub fn encryptor(self) -> encryptor::Encryptor<'a> {
        encryptor::Encryptor::new(self.original, self.source_data)
    }

    pub fn rebuild(self) -> Result<(bundle::Bundle, Box<[u8]>), Error> {
        if self.templates.is_empty() {
            // No signing to do
            return editor::Editor::new(self.original, self.source_data)
                .rebuild()
                .map_err(Into::into);
        }

        // Reorder and accumulate BIB operations
        let mut blocks = HashMap::new();
        for (block_number, template) in &self.templates {
            match blocks.entry((template.source.clone(), template.context.clone())) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(vec![(block_number, &template.key)]);
                }
                std::collections::hash_map::Entry::Occupied(mut e) => {
                    e.get_mut().push((block_number, &template.key));
                }
            }
        }

        let mut editor = editor::Editor::new(self.original, self.source_data);

        // Now build BIB blocks
        for ((source, context), contexts) in blocks {
            // Reserve a block number for the BIB block
            let new_block = block::Block {
                block_type: block::Type::BlockIntegrity,
                // TODO: set flags, crc, etc
                flags: block::Flags::default(),
                crc_type: crc::CrcType::None,
                extent: 0..0,
                data: 0..0,
                bib: None,
                bcb: None,
            };

            let b = editor
                .push_block(new_block.block_type)
                .expect("Failed to reserve block")
                .with_crc_type(new_block.crc_type)
                .with_flags(new_block.flags.clone());

            let source_block = b.block_number();
            editor = b.build([]);

            let editor_bs = editor::EditorBlockSet {
                editor,
                new_block,
                new_block_number: source_block,
            };

            let mut operation_set = bib::OperationSet {
                source: source.clone(),
                operations: HashMap::new(),
            };

            for (target_block, key) in contexts {
                operation_set.operations.insert(
                    *target_block,
                    build_bib_data(
                        &editor_bs,
                        &source,
                        context.clone(),
                        source_block,
                        *target_block,
                        key,
                    )?,
                );
            }

            // Rewrite with the real data
            editor = editor_bs
                .editor
                .update_block(source_block)
                .expect("Failed to update block")
                .build(hardy_cbor::encode::emit(&operation_set).0);
        }

        editor.rebuild().map_err(Into::into)
    }
}

#[allow(irrefutable_let_patterns)]
fn build_bib_data(
    editor: &editor::EditorBlockSet,
    source: &eid::Eid,
    context: Context,
    source_block: u64,
    target_block: u64,
    key: &key::Key,
) -> Result<bib::Operation, bpsec::Error> {
    let op_args = bib::OperationArgs {
        bpsec_source: source,
        target: target_block,
        source: source_block,
        blocks: editor,
    };

    #[cfg(feature = "rfc9173")]
    if let Context::HMAC_SHA2(scope_flags) = context {
        return Ok(bib::Operation::HMAC_SHA2(
            rfc9173::bib_hmac_sha2::Operation::sign(key, scope_flags, op_args)?,
        ));
    }

    panic!("Unsupported BIB context!");
}
