//! The send queue's entries.

use alloc::{boxed::Box, collections::VecDeque, vec::Vec};

use bytes::Bytes;
use smallvec::SmallVec;

#[cfg(doc)]
use super::{BundleFraming, SendHandle, SendKind, SendQueueHighWatermark, Sender};
use super::{id::LocalId, segmenter::QueuedTransfer};
use crate::codec::{encoded_message_len, hint::HintItem, message::Message};

/// A queue position: entries are packed in ascending order of the `Seq`
/// they were queued with.  A `u64` counter, so it never wraps in practice.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Seq(pub u64);

impl Seq {
    /// The position after this one.
    pub fn next(self) -> Self {
        Self(self.0 + 1)
    }
}

/// The entries of the packing order, ascending by [`Seq`].
///
/// A deque rather than a map: entries join at the back, since positions
/// are taken in order, and nearly all leave from the front, so both are
/// O(1), and a lookup by position is a binary search.  Only an entry
/// that leaves from behind others, or a parked transfer that returns,
/// shifts its neighbours, the fewer of those before or after it.
#[derive(Default)]
pub struct Order(VecDeque<(Seq, QueueEntry)>);

impl Order {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Where `seq` is, or else where it would go.  Positions are dense
    /// until an entry leaves from behind others, so the distance from the
    /// front is tried first.
    fn position(&self, seq: Seq) -> Result<usize, usize> {
        let front = self.0.front().map_or(seq, |&(front, _)| front);
        if let Some(i) = seq.0.checked_sub(front.0)
            && let Ok(i) = usize::try_from(i)
            && self.0.get(i).is_some_and(|&(at, _)| at == seq)
        {
            return Ok(i);
        }
        self.0.binary_search_by_key(&seq, |&(seq, _)| seq)
    }

    /// Put `entry` at `seq`, which no entry holds.
    pub fn insert(&mut self, seq: Seq, entry: QueueEntry) {
        let i = self.position(seq).expect_err("each position is taken once");
        self.0.insert(i, (seq, entry));
    }

    pub fn get(&self, seq: Seq) -> Option<&QueueEntry> {
        let i = self.position(seq).ok()?;
        Some(&self.0[i].1)
    }

    pub fn get_mut(&mut self, seq: Seq) -> Option<&mut QueueEntry> {
        let i = self.position(seq).ok()?;
        Some(&mut self.0[i].1)
    }

    pub fn remove(&mut self, seq: Seq) -> Option<QueueEntry> {
        let i = self.position(seq).ok()?;
        self.0.remove(i).map(|(_, entry)| entry)
    }

    /// The entries at `from` and after, with their positions.
    pub fn range_from(&self, from: Seq) -> impl Iterator<Item = (Seq, &QueueEntry)> {
        let i = self.position(from).unwrap_or_else(|i| i);
        self.0.range(i..).map(|(seq, entry)| (*seq, entry))
    }

    pub fn values(&self) -> impl Iterator<Item = &QueueEntry> {
        self.0.iter().map(|(_, entry)| entry)
    }
}

/// One bundle in the send queue.  No `Debug`: it holds bundle bytes.
pub enum QueueEntry {
    /// A Bundle Message and its ID, packed with its neighbours into PDUs.
    Message { id: LocalId, message: Message },
    /// A segmented transfer, cut into Transfer Segment and Transfer End
    /// messages as PDUs are packed.  Its ID is its transfer number.
    Transfer(Box<QueuedTransfer>),
    /// A bare bundle frame ([`BundleFraming::Bare`]) and the counter value
    /// of its [`SendKind::Bare`]: emitted as a PDU of its own, since
    /// nothing can precede, follow, or pad it.
    BareBundle { id: u32, data: Bytes },
}

impl QueueEntry {
    /// The ID of the bundle this entry holds.
    pub fn local_id(&self) -> LocalId {
        match self {
            Self::Message { id, .. } => *id,
            Self::Transfer(t) => LocalId::Transfer(t.transfer_number),
            Self::BareBundle { id, .. } => LocalId::Bare(*id),
        }
    }

    /// The segmented transfer the entry holds, if it holds one.
    pub fn as_transfer(&self) -> Option<&QueuedTransfer> {
        match self {
            Self::Transfer(t) => Some(t),
            _ => None,
        }
    }

    /// [`Self::as_transfer`], mutably.
    pub fn as_transfer_mut(&mut self) -> Option<&mut QueuedTransfer> {
        match self {
            Self::Transfer(t) => Some(t),
            _ => None,
        }
    }

    /// Whether the entry can supply its next message into `room` bytes of
    /// a PDU, `empty` if nothing has been packed into it yet.
    ///
    /// An empty PDU takes a whole message whatever its size, which is what
    /// guarantees progress, and is the only PDU a bare bundle frame, which
    /// is not a message and travels alone, can go in.  A transfer supplies
    /// a segment only once its bytes have been pushed (see
    /// [`QueuedTransfer::chunk_for`]).
    pub fn supplies(&self, room: usize, empty: bool) -> bool {
        match self {
            Self::Message { message, .. } => empty || encoded_message_len(message) <= room,
            Self::Transfer(t) => t.next_chunk(room).is_some(),
            Self::BareBundle { .. } => empty,
        }
    }

    /// The bundle bytes the entry holds, as [`SendQueueHighWatermark`] counts them.
    pub fn queued_bytes(&self) -> u64 {
        match self {
            Self::Message { message, .. } => match message {
                Message::Bundle { data, .. } => data.len() as u64,
                _ => 0,
            },
            Self::Transfer(t) => t.buffered as u64,
            Self::BareBundle { data, .. } => data.len() as u64,
        }
    }
}

/// A queued bundle as [`Sender::queued_from`] yields it: an entry of the
/// packing order, or a parked transfer.
#[derive(Clone, Copy)]
pub enum Queued<'a> {
    Entry(&'a QueueEntry),
    Parked(&'a QueuedTransfer),
}

impl<'a> Queued<'a> {
    /// The ID of the bundle.
    pub fn local_id(self) -> LocalId {
        match self {
            Self::Entry(entry) => entry.local_id(),
            Self::Parked(t) => LocalId::Transfer(t.transfer_number),
        }
    }

    /// The segmented transfer, if the bundle is one.
    pub fn as_transfer(self) -> Option<&'a QueuedTransfer> {
        match self {
            Self::Entry(entry) => entry.as_transfer(),
            Self::Parked(t) => Some(t),
        }
    }
}

/// A bundle that fits one PDU, being pushed through a [`SendHandle`]:
/// queued as a Bundle Message or bare frame when its last byte arrives.
pub struct Assembly {
    pub hints: Vec<HintItem>,
    /// The pushed chunks, gathered into one buffer when the last arrives.
    pub chunks: SmallVec<[Bytes; 1]>,
    /// The bytes in `chunks`.
    pub len: usize,
}

/// The encoded length of a Transfer Cancel.
pub fn cancel_message_len(transfer_number: u32) -> usize {
    encoded_message_len(&Message::TransferCancel { transfer_number })
}
