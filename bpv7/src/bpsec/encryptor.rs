use super::*;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("No such block number {0}")]
    NoSuchBlock(u64),

    #[error("Invalid block target {0}, BCB block")]
    InvalidTarget(u64),

    #[error("Block target {0} is already the target of a BCB")]
    AlreadyEncrypted(u64),

    #[error("Bundle is a fragment")]
    FragmentedBundle,

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
    AES_GCM(rfc9173::ScopeFlags),
}

impl Context {
    fn can_share(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(_) => false, // The presence of an IV in context parameters means operations MUST NOT be shared
        }
    }
}

struct BlockTemplate<'a> {
    context: Context,
    source: eid::Eid,
    key: &'a key::Key,
}

pub struct Encryptor<'a> {
    original: &'a bundle::Bundle,
    source_data: &'a [u8],
    templates: HashMap<u64, BlockTemplate<'a>>,
}

impl<'a> Encryptor<'a> {
    pub fn new(original: &'a bundle::Bundle, source_data: &'a [u8]) -> Self {
        Self {
            original,
            source_data,
            templates: HashMap::new(),
        }
    }

    /// Encrypt a block in the bundle.
    ///
    /// On error, returns the encryptor along with the error so it can be reused for recovery.
    #[allow(clippy::result_large_err)]
    pub fn encrypt_block(
        mut self,
        block_number: u64,
        context: Context,
        source: eid::Eid,
        key: &'a key::Key,
    ) -> Result<Self, (Self, Error)> {
        if self.original.flags.is_fragment {
            return Err((self, Error::FragmentedBundle));
        }

        if block_number == 0 {
            return Err((self, Error::InvalidTarget(block_number)));
        }

        let block = match self.original.blocks.get(&block_number) {
            Some(b) => b,
            None => return Err((self, Error::NoSuchBlock(block_number))),
        };

        if block.bcb.is_some() {
            return Err((self, Error::AlreadyEncrypted(block_number)));
        }

        if let block::Type::BlockSecurity = block.block_type {
            return Err((self, Error::InvalidTarget(block_number)));
        }

        /* RFC 9172 Section 3.9 states that BCBs targetting blocks with BIBs MUST also target the BIB
         * We take the 'all-or-nothing' approach and encrypt all BIB targets, rather than splitting the BIB
         * because splitting requires integrity keys */
        if let Some(bib_block) = block.bib {
            let bib = match self.original.blocks.get(&bib_block) {
                Some(b) => b,
                None => {
                    return Err((
                        self,
                        Error::Editor(editor::Error::Builder(builder::Error::InternalError(
                            crate::Error::Altered,
                        ))),
                    ));
                }
            };

            let bib_payload = match bib.payload(self.source_data) {
                Some(p) => p,
                None => {
                    return Err((
                        self,
                        Error::Editor(editor::Error::Builder(builder::Error::InternalError(
                            crate::Error::Altered,
                        ))),
                    ));
                }
            };

            let opset = match hardy_cbor::decode::parse::<bpsec::bib::OperationSet>(bib_payload) {
                Ok(opset) => opset,
                Err(e) => {
                    return Err((
                        self,
                        Error::Editor(editor::Error::Builder(builder::Error::InternalError(
                            crate::Error::InvalidField {
                                field: "BIB Abstract Syntax Block",
                                source: e.into(),
                            },
                        ))),
                    ));
                }
            };

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

        // Reorder and accumulate BCB operations if sharing is possible
        let mut bcbs = Vec::new();
        let mut shared_bcbs = HashMap::<(eid::Eid, Context), Vec<(u64, &'a key::Key)>>::new();
        for (block_number, template) in self.templates {
            if template.context.can_share() {
                shared_bcbs
                    .entry((template.source, template.context))
                    .or_default()
                    .push((block_number, template.key));
            } else {
                bcbs.push((
                    template.source,
                    template.context,
                    vec![(block_number, template.key)],
                ));
            }
        }

        // Add all shared BCBs to the total BCBs
        bcbs.extend(
            shared_bcbs
                .into_iter()
                .map(|((bpsec_source, context), targets)| (bpsec_source, context, targets)),
        );

        let mut editor = editor::Editor::new(self.original, self.source_data);

        // Now build BCB blocks
        for (bpsec_source, context, targets) in bcbs {
            /* RFC 9173, Section 4.8.1 states:
             * Prior to encryption, if a CRC value is present for the target block,
             * then that CRC value MUST be removed.  This requires removing the CRC
             * field from the target block and setting the CRC type field of the
             * target block to "no CRC is present." */
            for (target, _) in &targets {
                let target_block = self
                    .original
                    .blocks
                    .get(target)
                    .expect("Missing target block");
                if !matches!(target_block.crc_type, crc::CrcType::None) {
                    editor = editor
                        .update_block_inner(*target)
                        .map_err(|(_, e)| e)?
                        .with_crc_type(crc::CrcType::None)
                        .rebuild();
                }
            }

            // Reserve a block number for the BCB block
            let b = editor
                .push_block(block::Type::BlockSecurity)
                .map_err(|(_, e)| e)?
                .with_crc_type(crc::CrcType::None)
                .with_flags(block::Flags {
                    must_replicate: true,
                    ..Default::default()
                });

            let source = b.block_number();
            editor = b.rebuild();

            let mut editor_bs = editor::EditorBlockSet { editor };
            let mut operations = HashMap::new();
            for (target, key) in targets {
                let (op, data) = build_bcb_data(
                    context.clone(),
                    bcb::OperationArgs {
                        bpsec_source: &bpsec_source,
                        target,
                        source,
                        blocks: &editor_bs,
                    },
                    key,
                )?;

                // Rewrite the target block
                editor_bs.editor = editor_bs
                    .editor
                    .update_block_inner(target)
                    .map_err(|(_, e)| e)?
                    .with_data(data.into_vec().into())
                    .rebuild();

                operations.insert(target, op);
            }

            // Rewrite the BCB with the real data
            editor = editor_bs
                .editor
                .update_block_inner(source)
                .map_err(|(_, e)| e)?
                .with_data(
                    hardy_cbor::encode::emit(&bcb::OperationSet {
                        source: bpsec_source,
                        operations,
                    })
                    .0
                    .into(),
                )
                .rebuild();
        }

        editor.rebuild().map_err(Into::into)
    }
}

#[allow(irrefutable_let_patterns)]
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

    panic!("Unsupported BCB context!");
}
