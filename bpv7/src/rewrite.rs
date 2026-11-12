/*!
Applies queued rewrites (block removals and non-canonical re-emits) to a
structurally-parsed bundle, returning a fresh [`Bundle`]
and the chunk plan describing the new wire bytes.

This is the apply step that consumers reach for after composing the
the parser's classification and the keyed check in [`crate::bpsec::checks`] to decide
*what* to rewrite. The heavy lifting (the BPSec cascade through
BCB-encrypted BIBs) lives in [`crate::editor`] / `bpsec::edit`.
*/

use alloc::vec::Vec;

use crate::{
    Bundle, HashMap, HashSet,
    bpsec::edit::BPSecEditor,
    editor::{Chunk, Editor, Error},
};

/// Apply queued rewrites through `editor`. Bulk-removes via
/// [`BPSecEditor::remove_blocks`] (which handles cascading
/// through BCB-encrypted BIBs internally, with the editor's key source and
/// plaintext cache), then applies non-canonical re-emits.
///
/// Returns `Some((new_bundle, chunks))` when at least one block was
/// removed or re-emitted; returns `None` when the bundle holds none of the
/// requested blocks and there are no re-emits — caller can return `Valid`
/// instead of `Rewritten`.
///
/// Caller is responsible for the "nothing to do" short-circuit on
/// empty `to_update` + empty `to_remove`: don't call this in that case.
///
/// # Errors
///
/// The removal cascade's: a BCB-encrypted BIB it decrypts that does not
/// parse or fails its checks, or one whose targets it must strip but
/// cannot read, and its refusal of a request it cannot honour whole without
/// encrypting a BIB again, orphaning a block an unreadable BIB may list, or
/// breaking an operation that scopes the primary block
/// ([`BPSecEditor::remove_blocks`]).
///
/// # Panics
///
/// Panics if `to_update` names a block the bundle does not hold.
pub fn apply_rewrites(
    mut editor: Editor<'_>,
    to_update: HashMap<u64, Vec<u8>>,
    to_remove: HashSet<u64>,
) -> Result<Option<(Bundle, Vec<Chunk>)>, Error> {
    // Bulk-remove with full BPSec cascade.
    let removed_any = !to_remove.is_empty() && !editor.remove_blocks(&to_remove)?.is_empty();

    if !removed_any && to_update.is_empty() {
        return Ok(None);
    }

    // Non-canonical re-emits, of blocks the caller found in the bundle.
    for (block_number, payload) in to_update {
        editor
            .update_block_inner(block_number)
            .expect("to_update names blocks the bundle holds")
            .with_data(payload.into())
            .rebuild();
    }

    // Re-serialising a bundle the editor just assembled cannot fail.
    let (new_bundle, chunks) = editor
        .rebuild_bundle()
        .expect("the editor rebuilds the bundle it assembled");
    Ok(Some((new_bundle, chunks)))
}
