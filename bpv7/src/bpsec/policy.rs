use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;

use crate::block::Type as BlockType;
use crate::bpsec::encrypt::{self, aes_gcm};
use crate::bpsec::error::Error;
use crate::bpsec::key::{Key, KeySet};
use crate::bpsec::sign::{self, hmac_sha2};
use crate::bundle::{Bundle, ParsedBundle};
use crate::eid::Eid;

/// Selects a block by type rather than by block number.
#[derive(Debug, Clone)]
pub enum BlockSelector {
    /// The primary block (block 0).
    Primary,
    /// The payload block (block 1).
    Payload,
    /// A block by its type.
    ByType(BlockType),
    /// A block by its block number.
    ByNumber(u64),
}

impl BlockSelector {
    fn resolve(&self, bundle: &Bundle) -> Option<u64> {
        match self {
            Self::Primary => Some(0),
            Self::Payload => Some(1),
            Self::ByType(bt) => bundle
                .blocks
                .iter()
                .find(|(_, b)| b.block_type == *bt)
                .map(|(bn, _)| *bn),
            Self::ByNumber(bn) => {
                if bundle.blocks.contains_key(bn) {
                    Some(*bn)
                } else {
                    None
                }
            }
        }
    }
}

enum SecurityOperation<'a> {
    Sign {
        target: BlockSelector,
        scope_flags: hmac_sha2::ScopeFlags,
        source: Eid,
        key: &'a Key,
    },
    Encrypt {
        target: BlockSelector,
        scope_flags: aes_gcm::ScopeFlags,
        source: Eid,
        key: &'a Key,
    },
}

/// High-level security policy for applying BIB and BCB operations to a bundle.
///
/// Automatically enforces RFC 9172 Section 3.9 ordering (BIBs before BCBs).
#[derive(Default)]
pub struct SecurityPolicy<'a> {
    operations: Vec<SecurityOperation<'a>>,
}

impl<'a> SecurityPolicy<'a> {
    pub fn new() -> Self {
        Self {
            operations: Vec::new(),
        }
    }

    pub fn sign(
        mut self,
        target: BlockSelector,
        scope_flags: hmac_sha2::ScopeFlags,
        source: Eid,
        key: &'a Key,
    ) -> Self {
        self.operations.push(SecurityOperation::Sign {
            target,
            scope_flags,
            source,
            key,
        });
        self
    }

    pub fn encrypt(
        mut self,
        target: BlockSelector,
        scope_flags: aes_gcm::ScopeFlags,
        source: Eid,
        key: &'a Key,
    ) -> Self {
        self.operations.push(SecurityOperation::Encrypt {
            target,
            scope_flags,
            source,
            key,
        });
        self
    }

    pub fn apply(self, _bundle: &Bundle, data: &[u8]) -> Result<Box<[u8]>, Error> {
        let mut sign_ops = Vec::new();
        let mut encrypt_ops = Vec::new();

        for op in self.operations {
            match op {
                SecurityOperation::Sign { .. } => sign_ops.push(op),
                SecurityOperation::Encrypt { .. } => encrypt_ops.push(op),
            }
        }

        let mut current_data: Box<[u8]> = data.into();

        if !sign_ops.is_empty() {
            let parsed = ParsedBundle::parse_with_keys(&current_data, &KeySet::EMPTY)?;

            let mut targets = Vec::new();
            for op in sign_ops {
                if let SecurityOperation::Sign {
                    target,
                    scope_flags,
                    source,
                    key,
                } = op
                {
                    let bn = target
                        .resolve(&parsed.bundle)
                        .ok_or_else(|| Error::NoMatchingBlock(format!("{target:?}")))?;
                    targets.push((bn, scope_flags, source, key));
                }
            }

            current_data = sign::sign_with(&parsed.bundle, &current_data, &targets)?;
        }

        if !encrypt_ops.is_empty() {
            let parsed = ParsedBundle::parse_with_keys(&current_data, &KeySet::EMPTY)?;

            let mut targets = Vec::new();
            for op in encrypt_ops {
                if let SecurityOperation::Encrypt {
                    target,
                    scope_flags,
                    source,
                    key,
                } = op
                {
                    let bn = target
                        .resolve(&parsed.bundle)
                        .ok_or_else(|| Error::NoMatchingBlock(format!("{target:?}")))?;
                    targets.push((bn, scope_flags, source, key));
                }
            }

            current_data = encrypt::encrypt_with(&parsed.bundle, &current_data, &targets)?;
        }

        Ok(current_data)
    }
}
