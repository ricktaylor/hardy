//! Packed PDUs and the bundles they carry.

use alloc::vec::Vec;
use core::{
    fmt,
    iter::FusedIterator,
    ops::{Deref, Range},
    slice,
};

use bytes::{BufMut, Bytes};
use smallvec::SmallVec;

#[cfg(doc)]
use super::Sender;
use super::{Error, Result, SendId};

/// A bundle with bytes in a PDU, as listed by [`Pdu::carried`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Carried {
    /// The bundle, as [`Sender::enqueue`] reported it.
    pub id: SendId,
    /// Whether this PDU carries the bundle's last bytes: always for a
    /// Bundle Message or bare frame, and for a segmented bundle when the
    /// PDU holds its Transfer End.  Once the PDU holding it is written,
    /// the sender will emit nothing further for the bundle.
    pub completes: bool,
}

/// The [`Carried`] entries of one PDU, held inline up to
/// [`Self::INLINE`] entries and on the heap beyond that.
///
/// Dereferences to `[Carried]`.  Equality, hashing and `Debug` follow the
/// entries, not where they are stored.
///
/// A list keeps its heap buffer once it has one: when refilled by
/// [`Sender::next_pdu_into`] it writes into that buffer even for a PDU
/// whose entries would fit inline, so a reused list stops allocating once
/// it has grown to the largest PDU seen.  [`Self::with_capacity`] starts
/// with a heap buffer of a chosen size.
///
/// # Sizing
///
/// A PDU of valid BPv7 bundles carries at most `pdu_size / 32 +
/// window_size` entries.  The smallest valid bundle is 28 bytes (RFC 9171:
/// a 20-byte primary block with its mandatory CRC and every EID
/// `dtn:none`, an empty payload block, and the indefinite-length array
/// around them), so each Bundle Message is at least 32 bytes with its
/// header, and a PDU holds at most `pdu_size / 32` of them.  Each transfer
/// with segments in the PDU adds one entry, and at most `window_size`
/// transfers are outstanding.  The sender does not parse bundles; one
/// enqueued below 28 bytes can exceed the bound, at the cost of the list
/// growing.
///
/// The bound is loose for ordinary traffic.  Transfers are packed in queue
/// order unless one is passed over, so a PDU names more than two transfers
/// only when bundles are pushed in chunks and several wait on their
/// producers, or when transfers end within one PDU.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CarriedList(SmallVec<[Carried; CarriedList::INLINE]>);

impl CarriedList {
    /// Entries held without allocating.  A PDU lists one entry for each
    /// transfer with segments in it and one for each Bundle Message, so
    /// traffic of bundles longer than a quarter of the PDU, given whole to
    /// [`Sender::enqueue`], stays inline.
    pub const INLINE: usize = 4;

    /// An empty list, held inline.
    pub const fn new() -> Self {
        Self(SmallVec::new_const())
    }

    /// An empty list that holds `capacity` entries without allocating
    /// again: inline up to [`Self::INLINE`], otherwise in a heap buffer
    /// allocated now and kept for the list's life.
    pub fn with_capacity(capacity: usize) -> Self {
        Self(SmallVec::with_capacity(capacity))
    }

    /// Entries the list holds without allocating.
    pub fn capacity(&self) -> usize {
        self.0.capacity()
    }

    /// The bundles whose last bytes the PDU carries, in list order: those
    /// flagged [`Carried::completes`].  Once the PDU is written, each can be
    /// reported sent (see [`Sender::next_pdu`]).
    pub fn completed(&self) -> impl Iterator<Item = SendId> + '_ {
        self.iter().filter(|c| c.completes).map(|c| c.id)
    }

    /// Empty the list, keeping any heap buffer.
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    /// Append `item`, moving the entries to the heap if the inline slots
    /// are full.  A PDU names each bundle once.
    pub(super) fn push(&mut self, item: Carried) {
        debug_assert!(
            self.iter().all(|c| c.id != item.id),
            "a PDU names each bundle once"
        );
        self.0.push(item);
    }
}

impl Default for CarriedList {
    fn default() -> Self {
        Self::new()
    }
}

impl Deref for CarriedList {
    type Target = [Carried];

    fn deref(&self) -> &[Carried] {
        &self.0
    }
}

impl<'a> IntoIterator for &'a CarriedList {
    type Item = &'a Carried;
    type IntoIter = slice::Iter<'a, Carried>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl fmt::Debug for CarriedList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

/// A packed PDU and the bundles it carries, returned by [`Sender::next_pdu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdu {
    /// The PDU, ready for the link.
    pub data: Bytes,
    /// Every bundle with bytes in `data`, once each, in the order packed.
    /// Empty for a PDU holding only a Transfer Cancel.
    pub carried: CarriedList,
}

/// A packed PDU as a sequence of byte slices, filled by
/// [`Sender::next_pdu_vectored`] for a caller that gathers it into its own
/// memory or hands it to a vectored write.
///
/// The slices, in order, concatenate to exactly the bytes
/// [`Sender::next_pdu`] would have returned, and none is empty.  How the
/// PDU is split into slices, and which slices share the buffers bundles
/// were enqueued in rather than holding copied bytes, is unspecified and
/// may change between releases.
///
/// # Memory
///
/// A filled value holds references into the enqueued buffers its PDU
/// shares, keeping them alive until it is refilled, [`Self::clear`]ed, or
/// dropped.  Refilling or clearing keeps its own buffers, so a value reused
/// for every PDU stops allocating once it has grown to the largest PDU
/// seen.
#[derive(Default)]
pub struct PduSlices {
    /// Bytes copied into the PDU: message headers, padding, and any data
    /// not shared.
    copied: Vec<u8>,
    /// The PDU's slices, in order.
    spans: Vec<Span>,
    /// The PDU's length: the sum of the slices' lengths.
    len: usize,
}

/// One slice of a [`PduSlices`].
enum Span {
    /// A range of [`PduSlices::copied`].
    Copied(Range<usize>),
    /// Bytes shared with an enqueued buffer.
    Shared(Bytes),
}

impl PduSlices {
    /// The fewest bytes shared rather than copied: below this, copying
    /// costs less than counting a reference to the buffer and releasing it.
    pub(super) const SHARE_MIN: usize = 256;

    /// An empty PDU, holding no buffers.
    pub const fn new() -> Self {
        Self {
            copied: Vec::new(),
            spans: Vec::new(),
            len: 0,
        }
    }

    /// The PDU's length in bytes: the sum of the slices' lengths.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the PDU is empty, as it is when new, cleared, or after a
    /// call that packed nothing.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The PDU's slices, in order.
    pub fn iter(&self) -> PduSlicesIter<'_> {
        PduSlicesIter {
            copied: &self.copied,
            spans: self.spans.iter(),
        }
    }

    /// Copy the PDU to the start of `dst`, returning its length.  The rest
    /// of `dst` is left as it was.
    ///
    /// # Errors
    ///
    /// [`Error::BufferTooShort`] if `dst` is shorter than the PDU, leaving
    /// `dst` untouched.
    pub fn copy_to_slice(&self, dst: &mut [u8]) -> Result<usize> {
        let len = dst.len();
        let Some(mut out) = dst.get_mut(..self.len) else {
            return Err(Error::BufferTooShort {
                needed: self.len,
                len,
            });
        };
        for slice in self {
            out.put_slice(slice);
        }
        Ok(self.len)
    }

    /// Empty the PDU, releasing its references into enqueued buffers and
    /// keeping its own buffers.
    pub fn clear(&mut self) {
        self.copied.clear();
        self.spans.clear();
        self.len = 0;
    }

    /// Append the bytes `f` writes to the copied buffer, returning what it
    /// returns.
    pub(super) fn put_copied<R>(&mut self, f: impl FnOnce(&mut Vec<u8>) -> R) -> R {
        let start = self.copied.len();
        let r = f(&mut self.copied);
        let end = self.copied.len();
        if end > start {
            self.len += end - start;
            match self.spans.last_mut() {
                // Copied bytes are only appended, so the last copied span
                // ends where the new bytes start.
                Some(Span::Copied(range)) if range.end == start => range.end = end,
                _ => self.spans.push(Span::Copied(start..end)),
            }
        }
        r
    }

    /// Append the first `len` bytes of `bytes`, shared rather than copied
    /// unless shorter than [`Self::SHARE_MIN`].
    pub(super) fn put_shared(&mut self, bytes: &Bytes, len: usize) {
        if len < Self::SHARE_MIN {
            self.put_copied(|buf| buf.extend_from_slice(&bytes[..len]));
            return;
        }
        self.len += len;
        self.spans.push(Span::Shared(if len == bytes.len() {
            bytes.clone()
        } else {
            bytes.slice(..len)
        }));
    }
}

impl<'a> IntoIterator for &'a PduSlices {
    type Item = &'a [u8];
    type IntoIter = PduSlicesIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Summarises the PDU rather than printing its bytes.
impl fmt::Debug for PduSlices {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PduSlices")
            .field("len", &self.len)
            .field("slices", &self.spans.len())
            .finish()
    }
}

/// The slices of a [`PduSlices`], in order, returned by
/// [`PduSlices::iter`].
#[derive(Clone)]
pub struct PduSlicesIter<'a> {
    copied: &'a [u8],
    spans: slice::Iter<'a, Span>,
}

impl<'a> Iterator for PduSlicesIter<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        Some(match self.spans.next()? {
            Span::Copied(range) => &self.copied[range.clone()],
            Span::Shared(bytes) => bytes,
        })
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.spans.size_hint()
    }
}

impl FusedIterator for PduSlicesIter<'_> {}

/// Summarises the slices left rather than printing their bytes.
impl fmt::Debug for PduSlicesIter<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PduSlicesIter")
            .field("slices", &self.spans.len())
            .finish()
    }
}
