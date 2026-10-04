//! Packed PDUs and the bundles they carry.

use core::{fmt, ops::Deref, slice};

use bytes::Bytes;
use smallvec::SmallVec;

use super::SendId;
#[cfg(doc)]
use super::Sender;

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
