//! Revocation gossip in the Key Record Block (experimental block type 225,
//! dtn-custody-demo spec/KEY-RECORDS.md): an Egress [`Rewriter`] that learns
//! the revocation records arriving in any bundle's Key Record Block and adds
//! the ones it knows to every bundle it forwards, so news of a lost key
//! travels with ordinary traffic instead of waiting for a control channel.
//!
//! Records are COSE_Sign1 items; this node carries them, it does not judge
//! them. Verification stays with whoever acts on a key (the record names its
//! signer, and a verifier checks it against its own anchors). Carrying an
//! unverified revocation is safe: a forged one fails verification wherever it
//! lands, and the block is capped at [`MAX_RECORDS`].

use std::sync::Mutex;

use hardy_bpa::filter::{Boundary, RewriteContext, Rewriter};
use hardy_bpv7::{
    block::{Flags, Type},
    crc::CrcType,
    reader::Availability,
};
use tracing::debug;

use crate::receipt;

/// The experimental block-type code of the Key Record Block.
pub const BLOCK_TYPE: u64 = 225;

/// The most records one Key Record Block may hold.
pub const MAX_RECORDS: usize = 32;

// Record kind 2 is a revocation (key 1 of the record's payload map).
const KIND_REVOCATION: u64 = 2;

/// The record kind (payload map key 1) of one encoded COSE_Sign1 record.
#[must_use]
pub fn record_kind(item: &[u8]) -> Option<u64> {
    let payload = receipt::payload_of(item)?;
    let mut cursor = receipt::Cursor::new(payload);
    let (major, count) = cursor.head().ok()?;
    if major != 5 || count == 0 {
        return None;
    }
    match (cursor.head().ok()?, cursor.head().ok()?) {
        ((0, 1), (0, kind)) => Some(kind),
        _ => None,
    }
}

/// The revocation records in Key Record Block data.
#[must_use]
pub fn revocations(data: &[u8]) -> Vec<&[u8]> {
    receipt::items(data)
        .map(|items| {
            items
                .into_iter()
                .filter(|i| record_kind(i) == Some(KIND_REVOCATION))
                .collect()
        })
        .unwrap_or_default()
}

/// New Key Record Block data: the existing records unchanged and in order,
/// then each known record not already present, up to [`MAX_RECORDS`].
/// `None` when nothing would be added (or the existing data is malformed).
#[must_use]
pub fn merge(existing: Option<&[u8]>, known: &[Vec<u8>]) -> Option<Vec<u8>> {
    let mut items: Vec<&[u8]> = match existing {
        Some(data) => receipt::items(data).ok()?,
        None => Vec::new(),
    };
    let before = items.len();
    for record in known {
        if items.len() >= MAX_RECORDS {
            break;
        }
        if !items.contains(&record.as_slice()) {
            items.push(record);
        }
    }
    if items.len() == before {
        return None;
    }
    let mut out = Vec::new();
    receipt::array_head(&mut out, items.len() as u64);
    for item in items {
        out.extend_from_slice(item);
    }
    Some(out)
}

/// The gossiping Rewriter: one per node, holding the revocations it has seen.
#[derive(Debug, Default)]
pub struct KeyGossip {
    known: Mutex<Vec<Vec<u8>>>,
}

impl KeyGossip {
    /// The revocation records this node has learned, in the order it learned them.
    #[must_use]
    pub fn known(&self) -> Vec<Vec<u8>> {
        self.known.lock().map(|k| k.clone()).unwrap_or_default()
    }
}

impl Rewriter for KeyGossip {
    fn rewrite(&self, ctx: &mut RewriteContext<'_>) {
        if !matches!(ctx.boundary(), Boundary::Egress { .. })
            || ctx.bundle().primary.flags.is_admin_record
        {
            return;
        }
        let bundle = ctx.bundle();
        let reader = ctx.reader();
        let current = bundle
            .blocks
            .iter()
            .find(|(_, block)| block.block_type == Type::Unrecognised(BLOCK_TYPE))
            .map(|(number, _)| *number);
        let data = match current {
            Some(number) => match reader.block(number) {
                Some((_, Availability::Available(data))) => Some(data.as_ref().to_vec()),
                _ => return,
            },
            None => None,
        };
        let known = {
            let Ok(mut known) = self.known.lock() else {
                return;
            };
            if let Some(data) = &data {
                for record in revocations(data) {
                    if !known.iter().any(|k| k.as_slice() == record) && known.len() < MAX_RECORDS {
                        known.push(record.to_vec());
                    }
                }
            }
            known.clone()
        };
        let Some(merged) = merge(data.as_deref(), &known) else {
            return;
        };
        let edit = match current {
            Some(number) => ctx.editor().replace(number, merged.into()),
            None => ctx
                .editor()
                .insert(
                    Type::Unrecognised(BLOCK_TYPE),
                    Flags {
                        must_replicate: true,
                        ..Default::default()
                    },
                    CrcType::CRC32_CASTAGNOLI,
                    merged.into(),
                )
                .map(|_| ()),
        };
        if let Err(e) = edit {
            debug!("Key Record Block edit refused: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::slice::from_ref;

    use rand::{TryRng, rngs::SysRng};

    use super::*;

    // A COSE_Sign1-shaped record whose payload map is {1: kind, 10: h'tag'}.
    // Nothing here checks signatures, so the signature bytes are random filler.
    fn record(kind: u8, tag: u8) -> Vec<u8> {
        let payload = [0xa2, 0x01, kind, 0x0a, 0x41, tag];
        let mut signature = [0u8; 64];
        SysRng.try_fill_bytes(&mut signature).unwrap();
        let mut out = vec![0x84, 0x43, 0xa1, 0x01, 0x27, 0xa0, 0x46];
        out.extend_from_slice(&payload);
        out.extend_from_slice(&[0x58, 0x40]);
        out.extend_from_slice(&signature);
        out
    }

    #[test]
    fn kinds_are_read_and_only_revocations_are_learned() {
        let (key, rev) = (record(1, 1), record(2, 2));
        assert_eq!(record_kind(&key), Some(1));
        assert_eq!(record_kind(&rev), Some(2));
        let mut block = vec![0x82];
        block.extend_from_slice(&key);
        block.extend_from_slice(&rev);
        assert_eq!(revocations(&block), vec![rev.as_slice()]);
    }

    #[test]
    fn merge_keeps_existing_records_and_adds_the_rest_once() {
        let (a, b, c) = (record(2, 1), record(2, 2), record(1, 3));
        let mut existing = vec![0x82];
        existing.extend_from_slice(&c);
        existing.extend_from_slice(&a);
        let merged = merge(Some(&existing), &[a.clone(), b.clone()]).unwrap();
        assert_eq!(
            receipt::items(&merged).unwrap(),
            vec![c.as_slice(), a.as_slice(), b.as_slice()]
        );
        assert_eq!(merge(Some(&merged), &[a.clone(), b.clone()]), None);
        let fresh = merge(None, from_ref(&b)).unwrap();
        assert_eq!(receipt::items(&fresh).unwrap(), vec![b.as_slice()]);
        assert_eq!(merge(None, &[]), None);
    }

    #[test]
    fn the_block_is_capped() {
        let known: Vec<Vec<u8>> = (0..40).map(|i| record(2, i)).collect();
        let merged = merge(None, &known).unwrap();
        assert_eq!(receipt::items(&merged).unwrap().len(), MAX_RECORDS);
    }
}
