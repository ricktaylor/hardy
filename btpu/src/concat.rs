//! Joining buffers into one contiguous [`Bytes`].

use bytes::{Bytes, BytesMut};

/// `chunks` joined in order into one buffer of `len` bytes, their total
/// length.  A lone chunk is handed back as it is, with no copy.
pub fn concat<'a>(chunks: impl IntoIterator<Item = &'a Bytes>, len: usize) -> Bytes {
    let mut chunks = chunks.into_iter();
    let Some(first) = chunks.next() else {
        return Bytes::new();
    };
    let Some(second) = chunks.next() else {
        debug_assert_eq!(first.len(), len, "len is the chunks' total");
        return first.clone();
    };
    let mut data = BytesMut::with_capacity(len);
    data.extend_from_slice(first);
    data.extend_from_slice(second);
    for chunk in chunks {
        data.extend_from_slice(chunk);
    }
    debug_assert_eq!(data.len(), len, "len is the chunks' total");
    data.freeze()
}
