//! Segmented transfers: cutting segments from pushed chunks.

use alloc::{collections::VecDeque, vec::Vec};
use core::ops::RangeInclusive;

use bytes::{Buf, Bytes, BytesMut};

use super::SegmentCutStrategy;
#[cfg(doc)]
use super::{NextPduOptions, Sender};
use crate::codec::{encode_segment_head, hint::HintItem, segment_message_len};

/// The copies emitted of each segment.  Repetition (Section 6) would make
/// this a per-transfer setting; the queue already keeps a segment's
/// boundaries until its last copy.
pub const COPIES: u32 = 1;

/// A segmented bundle in the queue, from [`Sender::begin`] (or
/// [`Sender::enqueue`]) until the last copy of its End is packed.
///
/// Its segments are cut as PDUs are packed, each sized to the room left in
/// the PDU (see [`Self::chunk_for`]) and written straight from the chunks
/// pushed into it, so a queued transfer costs one queue entry and handles
/// on the pushed buffers rather than one message per segment.
pub struct QueuedTransfer {
    pub transfer_number: u32,
    /// The bundle's length, all pushed or not.
    total_len: usize,
    /// The pushed bytes from the cursor on, the segment with copies left
    /// included, in order.
    chunks: VecDeque<Bytes>,
    /// The bytes in `chunks`.
    pub buffered: usize,
    /// The hints for segment 0: the sender-derived Bundle Length followed
    /// by the caller's.  Dropped once segment 0's last copy is cut.
    first_segment_hints: Vec<HintItem>,
    /// Data bytes segment 0 may carry, reduced by its hints.
    first_capacity: usize,
    /// Data bytes every later segment may carry.
    capacity: usize,
    /// When a segment whose bytes are not all pushed is cut.
    segment_cut_strategy: SegmentCutStrategy,
    /// Bytes before the segment at the cursor.
    offset: usize,
    /// The index of the segment at the cursor.
    next_index: u32,
    /// The segment at the cursor, once its first copy is cut and while
    /// copies remain: its boundaries are fixed by the first copy, so every
    /// copy is byte-identical (Section 6).
    cut: Option<Cut>,
}

/// A segment with copies still to emit.
pub struct Cut {
    /// Its data bytes, from the transfer's cursor.
    len: usize,
    /// Copies still to emit, at least one.
    copies_left: u32,
}

impl QueuedTransfer {
    /// A transfer with nothing pushed yet.
    pub fn new(
        transfer_number: u32,
        total_len: usize,
        first_segment_hints: Vec<HintItem>,
        first_capacity: usize,
        capacity: usize,
        segment_cut_strategy: SegmentCutStrategy,
    ) -> Self {
        Self {
            transfer_number,
            total_len,
            chunks: VecDeque::new(),
            buffered: 0,
            first_segment_hints,
            first_capacity,
            capacity,
            segment_cut_strategy,
            offset: 0,
            next_index: 0,
            cut: None,
        }
    }

    /// Whether the next segment is waiting on bytes not yet pushed: it
    /// could not go even in an empty PDU of `pdu_size` bytes.
    pub fn waiting(&self, pdu_size: usize) -> bool {
        self.cut.is_none()
            && self
                .chunk_for(self.offset, self.next_index, pdu_size)
                .is_none()
    }

    /// Whether the next segment cannot go in a room of any size until more
    /// bytes are pushed, so the queue can set the transfer aside.
    ///
    /// Stricter than [`Self::waiting`]: a segment waiting for all of its
    /// bytes may still fill the tail of a PDU with the bytes it has (see
    /// [`Self::chunk_for`]), but no segment is cut shorter than half its
    /// capacity unless it ends the bundle, so one with fewer bytes pushed
    /// than that, and than the rest of the bundle, can go nowhere.  Only a
    /// push or a cut changes the answer.
    pub fn starved(&self) -> bool {
        if self.cut.is_some() || self.finished() {
            return false;
        }
        let capacity = self.capacity_at(self.next_index);
        let whole = (self.total_len - self.offset).min(capacity);
        self.buffered < whole.min(capacity.div_ceil(2))
    }

    /// The rooms smaller than `room` that the next segment could go in,
    /// given that it cannot go in `room`, or `None` if there are none.
    ///
    /// Shrinking the room helps only a segment that waits for all of its
    /// bytes and could fill a tail with the bytes pushed (see
    /// [`Self::chunk_for`]): it goes in any room whose data bytes are at
    /// least half its capacity and at most what is pushed.  A segment
    /// already cut has a fixed length, so a room too small for it leaves no
    /// smaller room that is not.
    pub fn retry_rooms(&self, room: usize) -> Option<RangeInclusive<usize>> {
        if self.cut.is_some() {
            return None;
        }
        let index = self.next_index;
        let fits = self.data_room(index, room)?;
        let capacity = self.capacity_at(index);
        let half = capacity.div_ceil(2);
        let most = self.buffered.min(fits.checked_sub(1)?);
        (most >= half).then(|| self.segment_len(index, half)..=self.segment_len(index, most))
    }

    /// Whether any segment has been cut (and so packed into a PDU).
    pub fn started(&self) -> bool {
        self.offset > 0 || self.cut.is_some()
    }

    /// Whether the last copy of every segment has been cut, the Transfer
    /// End included.
    pub fn finished(&self) -> bool {
        self.offset >= self.total_len
    }

    /// Append pushed bytes.
    pub fn push(&mut self, chunk: Bytes) {
        self.buffered += chunk.len();
        self.chunks.push_back(chunk);
    }

    /// The hints segment `index` carries.
    pub fn hints_at(&self, index: u32) -> &[HintItem] {
        if index == 0 {
            &self.first_segment_hints
        } else {
            &[]
        }
    }

    /// The most data bytes segment `index` may carry.
    pub fn capacity_at(&self, index: u32) -> usize {
        if index == 0 {
            self.first_capacity
        } else {
            self.capacity
        }
    }

    /// The encoded length of segment `index` carrying `len` data bytes.
    pub fn segment_len(&self, index: u32, len: usize) -> usize {
        segment_message_len(self.hints_at(index), len)
    }

    /// The data bytes segment `index` fits in `room` bytes of a PDU, or
    /// `None` if not even its framing fits.
    pub fn data_room(&self, index: u32, room: usize) -> Option<usize> {
        room.checked_sub(self.segment_len(index, 0))
    }

    /// The data bytes of a segment not yet cut, at `offset` with index
    /// `index`, placed in `room` bytes of a PDU, or `None` if it cannot go
    /// there.
    ///
    /// A segment carries what remains of the bundle up to its capacity.  If
    /// that does not fit, the segment fills the room instead, but only if
    /// the room holds at least half its capacity: the segment count then
    /// stays within twice the full-size count, and every segment but the
    /// last fills at least half a PDU.  If not all of those bytes have been
    /// pushed, the segment waits for them, or under [`SegmentCutStrategy::Half`]
    /// carries what has been pushed if that is at least half its capacity,
    /// which keeps the same bounds.  An empty PDU always has room for a
    /// full-size segment, since `plan` sizes the capacities to fit one.
    pub fn chunk_for(&self, offset: usize, index: u32, room: usize) -> Option<usize> {
        let fits = self.data_room(index, room)?;
        let capacity = self.capacity_at(index);
        let whole = (self.total_len - offset).min(capacity);
        let len = if whole <= fits {
            whole
        } else if fits >= capacity.div_ceil(2) {
            fits
        } else {
            return None;
        };
        let pushed = self.offset + self.buffered - offset;
        if len <= pushed {
            Some(len)
        } else if self.segment_cut_strategy == SegmentCutStrategy::Half
            && pushed >= capacity.div_ceil(2)
        {
            Some(pushed)
        } else {
            None
        }
    }

    /// The data bytes the next segment message carries in `room` bytes, or
    /// `None` if it cannot go there.
    pub fn next_chunk(&self, room: usize) -> Option<usize> {
        match &self.cut {
            Some(cut) => (self.segment_len(self.next_index, cut.len) <= room).then_some(cut.len),
            None => self.chunk_for(self.offset, self.next_index, room),
        }
    }

    /// The data bytes a flush cuts from this transfer into `room` bytes of
    /// a PDU of `pdu_size`, or `None` if it cuts nothing (see
    /// [`NextPduOptions::flush`]).
    ///
    /// Only a transfer whose next segment is waiting on its producer is
    /// flushed.  The segment carries what has been pushed, as much as fits,
    /// at least one byte.  The rest of the bundle, cut normally from then
    /// on, needs at most one index per half segment (see `chunk_for`), so
    /// the cut is refused if that could reach `u32::MAX`, as `plan` refuses
    /// a bundle.  The rest is never empty: a waiting segment has fewer
    /// bytes pushed than it would carry.
    pub fn flush_chunk(&self, room: usize, pdu_size: usize) -> Option<usize> {
        if !self.waiting(pdu_size) {
            return None;
        }
        let len = self.buffered.min(self.data_room(self.next_index, room)?);
        if len == 0 {
            return None;
        }
        let rest = (self.total_len - self.offset - len) as u64;
        let last_index =
            u64::from(self.next_index) + rest.div_ceil(self.capacity.div_ceil(2) as u64);
        (last_index < u64::from(u32::MAX)).then_some(len)
    }

    /// The encoded length of the segment messages the transfer supplies to
    /// `room` bytes of a PDU, as [`Sender::pack`] would cut them.
    pub fn planned_len(&self, room: usize) -> usize {
        let (mut offset, mut index, mut total) = (self.offset, self.next_index, 0);
        if let Some(cut) = &self.cut {
            total = self.segment_len(index, cut.len);
            if total > room {
                return 0;
            }
            offset += cut.len;
            index = index.wrapping_add(1);
        }
        while offset < self.total_len {
            let Some(chunk) = self.chunk_for(offset, index, room - total) else {
                break;
            };
            total += self.segment_len(index, chunk);
            offset += chunk;
            index = index.wrapping_add(1);
        }
        total
    }

    /// Write the next segment message, carrying `len` data bytes, into
    /// `dst`, and return the pushed bytes it releases.  The caller sizes
    /// `len` with [`Self::next_chunk`] or [`Self::flush_chunk`].  The last
    /// segment is a Transfer End.  Bytes are released only with a segment's
    /// last copy.
    pub fn cut(&mut self, len: usize, dst: &mut BytesMut) -> usize {
        let index = self.next_index;
        let end = self.offset + len == self.total_len;
        encode_segment_head(
            end,
            self.transfer_number,
            index,
            self.hints_at(index),
            len,
            dst,
        )
        .expect("segments are sized against the PDU");
        let mut left = len;
        for chunk in &self.chunks {
            let n = left.min(chunk.len());
            dst.extend_from_slice(&chunk[..n]);
            left -= n;
            if left == 0 {
                break;
            }
        }

        let copies_left = self.cut.take().map_or(COPIES, |cut| cut.copies_left) - 1;
        if copies_left > 0 {
            self.cut = Some(Cut { len, copies_left });
            return 0;
        }
        if index == 0 {
            self.first_segment_hints = Vec::new();
        }
        self.release(len);
        self.offset += len;
        self.next_index = index.wrapping_add(1);
        len
    }

    /// Drop `len` bytes from the front of `chunks`.
    pub fn release(&mut self, len: usize) {
        self.buffered -= len;
        let mut left = len;
        while left > 0 {
            let front = self
                .chunks
                .front_mut()
                .expect("released bytes are buffered");
            if front.len() > left {
                front.advance(left);
                break;
            }
            left -= front.len();
            self.chunks.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::*;
    use crate::codec::{
        decode_pdu,
        hint::encoded_hints_len,
        message::{Message, SEGMENT_FRAMING},
    };

    // The room in a PDU that holds a full segment of 40 data bytes.
    const PDU: usize = SEGMENT_FRAMING + 40;

    // A transfer of `total_len` patterned bytes, of which those from
    // `offset` on are pushed, its cursor at `offset` and segment `index`.
    fn transfer_at(total_len: usize, offset: usize, index: u32) -> QueuedTransfer {
        let mut t = QueuedTransfer::new(7, total_len, Vec::new(), 40, 40, SegmentCutStrategy::Full);
        t.push((offset..total_len).map(|i| i as u8).collect());
        t.offset = offset;
        t.next_index = index;
        t
    }

    // Every state of a transfer cut for `PDU` that the starved and retry
    // predicates distinguish: both strategies, segment 0 with and without
    // hints and a later segment, a last segment shorter than, equal to and
    // longer than a full one, every amount pushed, and a segment with a
    // copy left to emit.
    fn transfer_states() -> Vec<QueuedTransfer> {
        let mut states = Vec::new();
        for strategy in [SegmentCutStrategy::Full, SegmentCutStrategy::Half] {
            for total_len in [1, 20, 39, 40, 41, 100] {
                for (offset, hinted) in [(0, false), (0, true), (40, false)] {
                    if offset > total_len {
                        continue;
                    }
                    let hints = if hinted {
                        vec![HintItem::BundleLength(total_len as u64)]
                    } else {
                        Vec::new()
                    };
                    let first_capacity = 40 - encoded_hints_len(&hints);
                    for pushed in 0..=total_len - offset {
                        let mut t = QueuedTransfer::new(
                            7,
                            total_len,
                            hints.clone(),
                            first_capacity,
                            40,
                            strategy,
                        );
                        t.push(Bytes::from(vec![0; pushed]));
                        t.offset = offset;
                        t.next_index = u32::from(offset > 0);
                        states.push(t);
                    }
                }
            }
        }
        let mut t = transfer_at(100, 50, 2);
        t.cut = Some(Cut {
            len: 25,
            copies_left: 1,
        });
        states.push(t);
        states
    }

    #[test]
    fn a_starved_transfer_fits_no_room_and_any_other_fits_some() {
        for t in transfer_states() {
            let fits_some = (0..=PDU).any(|room| t.next_chunk(room).is_some());
            assert_eq!(
                t.starved(),
                !fits_some,
                "total {} offset {} pushed {} strategy {:?}",
                t.total_len,
                t.offset,
                t.buffered,
                t.segment_cut_strategy
            );
        }
    }

    #[test]
    fn a_transfer_passed_over_fits_exactly_its_retry_rooms() {
        for t in transfer_states() {
            for room in (0..=PDU).filter(|&room| t.next_chunk(room).is_none()) {
                let retry = t.retry_rooms(room);
                if let Some(retry) = &retry {
                    assert!(*retry.end() < room);
                }
                for smaller in 0..room {
                    assert_eq!(
                        t.next_chunk(smaller).is_some(),
                        retry.as_ref().is_some_and(|r| r.contains(&smaller)),
                        "total {} offset {} pushed {} strategy {:?} room {room} smaller {smaller}",
                        t.total_len,
                        t.offset,
                        t.buffered,
                        t.segment_cut_strategy
                    );
                }
            }
        }
    }

    #[test]
    fn a_last_segment_takes_any_tail_it_fits_and_a_cut_keeps_its_length() {
        // Five bytes remain, well under half a segment, and still fit.
        let mut t = transfer_at(100, 95, 2);
        assert_eq!(t.chunk_for(95, 2, SEGMENT_FRAMING + 5), Some(5));
        assert_eq!(t.chunk_for(95, 2, SEGMENT_FRAMING + 4), None);

        // A segment with copies left goes out at its first copy's length or
        // not at all, whatever the room.  Only repetition (Section 6) leaves
        // copies to emit; with `COPIES` at 1 the sender never reaches this
        // state, and the check keeps the path ready for it.
        t = transfer_at(100, 50, 2);
        t.cut = Some(Cut {
            len: 25,
            copies_left: 1,
        });
        assert_eq!(t.next_chunk(SEGMENT_FRAMING + 40), Some(25));
        assert_eq!(t.next_chunk(SEGMENT_FRAMING + 24), None);
        let mut buf = BytesMut::new();
        assert_eq!(t.cut(25, &mut buf), 25);
        let Some(Ok(Message::TransferSegment(m))) = decode_pdu(buf.freeze()).next() else {
            panic!("expected a segment");
        };
        assert_eq!(m.segment_index, 2);
        assert_eq!(m.data, (50..75).map(|i| i as u8).collect::<Vec<_>>());
        assert_eq!((t.offset, t.next_index, t.cut.is_none()), (75, 3, true));
        assert_eq!(t.buffered, 25);
    }

    #[test]
    fn a_segment_waits_for_all_of_its_bytes() {
        let mut t = QueuedTransfer::new(7, 100, Vec::new(), 40, 40, SegmentCutStrategy::Full);
        assert_eq!(t.next_chunk(SEGMENT_FRAMING + 40), None);
        // Enough for a half-PDU tail segment, but the segment would be cut
        // to what has arrived rather than to the room.
        t.push(Bytes::from(vec![0; 30]));
        assert_eq!(t.next_chunk(SEGMENT_FRAMING + 40), None);
        assert_eq!(t.next_chunk(SEGMENT_FRAMING + 30), Some(30));
        // A segment spanning two pushed chunks is copied from both, and
        // only the bytes it carries are released.
        t.push(Bytes::from(vec![1; 30]));
        let mut buf = BytesMut::new();
        assert_eq!(t.cut(40, &mut buf), 40);
        assert_eq!(t.buffered, 20);
        assert_eq!(t.chunks.len(), 1);
        let Some(Ok(Message::TransferSegment(m))) = decode_pdu(buf.freeze()).next() else {
            panic!("expected a segment");
        };
        assert_eq!(&m.data[..], [[0; 30].as_slice(), &[1; 10]].concat());
    }

    #[test]
    fn a_flush_is_refused_if_the_rest_could_need_index_u32_max() {
        // Ten of 100 bytes pushed: a flush cuts all ten, leaving 90 bytes,
        // which could take five more segments of at least half of 40.
        let mut t = QueuedTransfer::new(7, 100, Vec::new(), 40, 40, SegmentCutStrategy::Full);
        t.push(Bytes::from(vec![0; 10]));
        t.next_index = u32::MAX - 6;
        assert_eq!(
            t.flush_chunk(SEGMENT_FRAMING + 40, SEGMENT_FRAMING + 40),
            Some(10)
        );
        // Room for fewer bytes than are pushed cuts what fits.
        assert_eq!(
            t.flush_chunk(SEGMENT_FRAMING + 4, SEGMENT_FRAMING + 40),
            Some(4)
        );
        assert_eq!(t.flush_chunk(SEGMENT_FRAMING, SEGMENT_FRAMING + 40), None);
        // One index later the last segment could be u32::MAX.
        t.next_index = u32::MAX - 5;
        assert_eq!(
            t.flush_chunk(SEGMENT_FRAMING + 40, SEGMENT_FRAMING + 40),
            None
        );
    }
}
