use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy, Hash, Eq, PartialEq)]
pub enum Context {
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2,
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
    ) -> Self {
        self.templates.insert(
            block_number,
            BlockTemplate {
                context,
                source,
                key,
            },
        );
        self
    }

    pub fn rebuild(self) -> Result<(bundle::Bundle, Box<[u8]>), bpsec::Error> {
        if self.templates.is_empty() {
            // No signing to do
            return Ok(editor::Editor::new(self.original, self.source_data).rebuild());
        }

        // Reorder and accumulate BIB operations
        let mut blocks = HashMap::new();
        for (block_number, template) in &self.templates {
            match blocks.entry((template.source.clone(), template.context)) {
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
            let b = editor
                .push_block(block::Type::BlockIntegrity)
                .expect("Failed to reserve block");

            // TODO: set flags, crc, etc
            let source_block = b.block_number();
            editor = b.build([]);

            let mut operation_set = bib::OperationSet {
                source: source.clone(),
                operations: HashMap::new(),
            };

            for (target_block, key) in contexts {
                operation_set.operations.insert(
                    *target_block,
                    self.build_bib_data(&source, &context, source_block, *target_block, key)?,
                );
            }

            // Rewrite with the real data
            editor = editor
                .update_block(source_block)
                .expect("Failed to update block")
                .build(hardy_cbor::encode::emit(&operation_set).0);
        }

        Ok(editor.rebuild())
    }

    #[allow(irrefutable_let_patterns)]
    fn build_bib_data(
        &self,
        source: &eid::Eid,
        context: &Context,
        source_block: u64,
        target_block: u64,
        key: &key::Key,
    ) -> Result<bib::Operation, bpsec::Error> {
        #[cfg(feature = "rfc9173")]
        if let Context::HMAC_SHA2 = context {
            return Ok(bib::Operation::HMAC_SHA2(
                rfc9173::bib_hmac_sha2::Operation::sign(
                    key,
                    bib::OperationArgs {
                        bpsec_source: source,
                        target: target_block,
                        source: source_block,
                        blocks: self,
                    },
                )?,
            ));
        }

        panic!("Unsupported BIB context!");
    }
}

impl<'a> bpsec::BlockSet<'a> for Signer<'a> {
    fn block(&'a self, block_number: u64) -> Option<&'a block::Block> {
        self.original.blocks.get(&block_number)
    }

    fn block_payload(&'a self, block_number: u64) -> Option<&'a [u8]> {
        Some(&self.source_data[self.original.blocks.get(&block_number)?.payload()])
    }
}
