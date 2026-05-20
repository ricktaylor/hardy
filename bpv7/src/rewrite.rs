/*!
Applies queued rewrites (block removals and non-canonical re-emits) to a
structurally-parsed bundle, returning a fresh [`Bundle`](crate::bundle::Bundle)
and the chunk plan describing the new wire bytes.

This is the apply step that consumers reach for after composing the
classification / decrypt / verify primitives in [`crate::checks`] to decide
*what* to rewrite. The heavy lifting (the BPSec cascade through
BCB-encrypted BIBs) lives in [`crate::editor`] / `bpsec::edit`.
*/

use super::*;
use editor::{Chunk, Editor};

/// Apply queued rewrites. Bulk-removes via
/// [`bpsec::edit::BPSecEditor::remove_blocks`] (which handles cascading
/// through BCB-encrypted BIBs internally), then applies non-canonical
/// re-emits.
///
/// Returns `Some((new_bundle, chunks))` when at least one block was
/// removed or re-emitted; returns `None` when the cascade silently
/// pulled all requested targets back (no Encrypt key) and there were
/// no re-emits — caller can return `Valid` instead of `Rewritten`.
///
/// Caller is responsible for the "nothing to do" short-circuit on
/// empty `to_update` + empty `to_remove`: don't call this in that case.
#[allow(clippy::result_large_err)]
pub fn apply_rewrites<'a>(
    data: &'a [u8],
    bundle: &'a Bundle,
    key_source: &dyn bpsec::key::KeySource,
    to_update: HashMap<u64, Vec<u8>>,
    to_remove: HashSet<u64>,
) -> Result<Option<(Bundle, Vec<Chunk>)>, Error> {
    use bpsec::edit::BPSecEditor;

    let mut editor = Editor::new(bundle, data);

    // Bulk-remove with full BPSec cascade. Lenient: any covered BIB that
    // would be partially shrunk without an available Encrypt key has its
    // targets silently retained.
    let removed_any = if to_remove.is_empty() {
        false
    } else {
        let (ed, removed) = editor
            .remove_blocks(to_remove, key_source)
            .unwrap_or_else(|(_, e)| {
                panic!("bpsec::edit::remove_blocks failed on parsed bundle: {e}")
            });
        editor = ed;
        !removed.is_empty()
    };

    if !removed_any && to_update.is_empty() {
        return Ok(None);
    }

    // Non-canonical re-emits.
    for (block_number, payload) in to_update {
        editor = editor
            .update_block_inner(block_number)
            .unwrap_or_else(|(_, e)| panic!("Editor update_block failed on parsed bundle: {e}"))
            .with_data(payload.into())
            .rebuild();
    }

    let (new_bundle, chunks) = editor
        .rebuild_bundle()
        .unwrap_or_else(|e| panic!("Editor rebuild failed on parsed bundle: {e}"));
    Ok(Some((new_bundle, chunks)))
}
