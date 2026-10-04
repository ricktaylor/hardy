//! Packing the queue into PDUs.

use alloc::vec::Vec;
use core::ops::RangeInclusive;

use bytes::{Bytes, BytesMut};

use super::{
    Carried, CarriedList, NextPduOptions, Pdu, SendId, Sender,
    id::LocalId,
    queue::{QueueEntry, Seq, cancel_message_len},
};
#[cfg(doc)]
use super::{LinkFraming, segmenter::QueuedTransfer};
use crate::codec::{encode_message, encoded_message_len, message::Message, pad_pdu};

impl Sender {
    /// Pack pending messages into a PDU of at most `pdu_size` bytes.
    ///
    /// Returns `None` if nothing is ready: the queue is empty, or holds
    /// only transfers whose next segments wait on bytes not yet pushed (and
    /// with [`NextPduOptions::flush`], none of them has bytes buffered).
    /// Entries are packed in queue order, but a transfer that cannot supply
    /// its next segment, for want of bytes or of room, is passed over, so
    /// a bundle still being pushed does not hold up the queue behind it.
    /// Under [`LinkFraming::FixedSize`] the PDU is padded to exactly `pdu_size`
    /// bytes; under [`LinkFraming::Variable`] it holds the packed messages,
    /// padded up to `min_pdu_len` if they are shorter.  A queued bare
    /// bundle frame, never shorter than `min_pdu_len`, is returned as-is,
    /// on its own, sharing the enqueued buffer.
    ///
    /// [`Pdu::carried`] names every bundle with bytes in the PDU and flags
    /// those whose last bytes it carries.  A CLA that reports per-bundle
    /// outcomes can treat a bundle as sent once the PDU flagging it
    /// [`Carried::completes`] is written, and as failed if it
    /// [`Self::cancel`]s it first (a cancelled bundle is never flagged) or
    /// a write of any PDU carrying it fails.  [`Self::next_pdu_into`] does
    /// the same into a reused list.
    ///
    /// The list starts inline, so it allocates only for a PDU carrying
    /// more than [`CarriedList::INLINE`] bundles.
    ///
    /// Packing a Transfer End releases its transfer's window slot (see
    /// [`Sender`]).
    #[must_use = "a PDU is packed once; dropping it loses what it carries"]
    pub fn next_pdu(&mut self) -> Option<Pdu> {
        self.next_pdu_with(NextPduOptions::default())
    }

    /// [`Self::next_pdu`], packed as `options` asks.
    #[must_use = "a PDU is packed once; dropping it loses what it carries"]
    pub fn next_pdu_with(&mut self, options: NextPduOptions) -> Option<Pdu> {
        let mut carried = CarriedList::new();
        let data = self.next_pdu_into_with(&mut carried, options)?;
        Some(Pdu { data, carried })
    }

    /// [`Self::next_pdu`], writing the carried bundles into `carried`
    /// rather than a new list, so a caller draining a busy link allocates
    /// nothing for the list once it has grown to the largest PDU seen, or
    /// nothing at all if created with
    /// [`CarriedList::with_capacity`]`(pdu_size / 32 + window_size)` (see
    /// [`CarriedList`] for that bound).
    ///
    /// `carried` is cleared first, keeping its heap buffer if it has one,
    /// so after the call it holds exactly this PDU's entries, and is left
    /// empty when `None` is returned.
    #[must_use = "a PDU is packed once; dropping it loses what it carries"]
    pub fn next_pdu_into(&mut self, carried: &mut CarriedList) -> Option<Bytes> {
        self.next_pdu_into_with(carried, NextPduOptions::default())
    }

    /// [`Self::next_pdu_into`], packed as `options` asks.
    #[must_use = "a PDU is packed once; dropping it loses what it carries"]
    pub fn next_pdu_into_with(
        &mut self,
        carried: &mut CarriedList,
        options: NextPduOptions,
    ) -> Option<Bytes> {
        carried.clear();
        let pdu = self.pack(carried, options)?;

        // Draining frees send-queue capacity (and possibly a window slot);
        // wake any task parked on `poll_ready`.
        self.wake_enqueue();

        Some(pdu)
    }

    /// Pack the queue's next PDU, recording the bundles it carries in
    /// `carried`, or return `None` if nothing is ready.
    ///
    /// The loop is driven by the queue, not by a count planned up front:
    /// [`Self::next_source`] names the entry that supplies each message,
    /// and an entry leaves the queue only once its last message is packed.
    /// The first message always goes in, so every PDU makes progress.
    /// Nothing larger than an empty PDU is ever queued (a Bundle Message is
    /// only made for a bundle that fits with its hints, `plan` sizes a
    /// transfer's full segments to fit or refuses the bundle, and a
    /// Transfer Cancel is smaller than any segment of the transfer it
    /// cancels), but were it otherwise the message would go out as one
    /// oversized PDU rather than stall the queue.
    fn pack(&mut self, carried: &mut CarriedList, options: NextPduOptions) -> Option<Bytes> {
        let pdu_size = self.pdu_size.get();
        let mut scan = Scan::default();
        let ready = self.next_source(&mut scan, 0, pdu_size).is_some()
            || (options.flush && self.flush_source(Seq(0), 0, pdu_size).is_some());
        #[cfg(test)]
        {
            self.probes += scan.probes;
        }
        if !ready {
            return None;
        }
        let padded_len = self.padded_len();
        let mut buf = BytesMut::with_capacity(if padded_len == pdu_size {
            pdu_size
        } else {
            self.planned_len(pdu_size).max(padded_len)
        });
        let owner = self.owner;
        let mut scan = Scan::default();
        while let Some(source) = self.next_source(&mut scan, buf.len(), pdu_size) {
            let seq = match source {
                Source::Cancel => {
                    let transfer_number = self.cancels.pop_front().expect("named by next_source");
                    encode_queued(&Message::TransferCancel { transfer_number }, &mut buf);
                    continue;
                }
                Source::Entry(seq) => seq,
            };
            let entry = self.order.get_mut(seq).expect("named by next_source");
            let consumed = match entry {
                QueueEntry::BareBundle { id, data } => {
                    // Chosen only for an empty PDU; returned as-is, alone.
                    carried.push(Carried {
                        id: SendId::new(owner, LocalId::Bare(*id)),
                        completes: true,
                    });
                    let data = data.clone();
                    self.dequeue(seq);
                    return Some(data);
                }
                QueueEntry::Message { id, message } => {
                    carried.push(Carried {
                        id: SendId::new(owner, *id),
                        completes: true,
                    });
                    encode_queued(message, &mut buf);
                    true
                }
                QueueEntry::Transfer(t) => {
                    let len = t
                        .next_chunk(pdu_size.saturating_sub(buf.len()))
                        .expect("next_source checked that it fits");
                    self.queued_bytes -= t.cut(len, &mut buf) as u64;
                    let completes = t.finished();
                    // A transfer supplies one segment to a PDU: the cut
                    // either fills the room, ends the transfer, or under
                    // `SegmentCutStrategy::Half` takes every byte buffered.
                    carried.push(Carried {
                        id: SendId::new(owner, LocalId::Transfer(t.transfer_number)),
                        completes,
                    });
                    if completes {
                        // The transfer's End is packed; nothing further will
                        // be emitted for it, so its slot is free.
                        self.allocator.release(t.transfer_number);
                    }
                    debug_assert!(
                        completes || t.next_chunk(pdu_size.saturating_sub(buf.len())).is_none(),
                        "a transfer supplies one segment to a PDU"
                    );
                    completes
                }
            };
            if consumed {
                self.dequeue(seq);
            } else {
                self.settle(seq);
            }
        }
        #[cfg(test)]
        {
            self.probes += scan.probes;
        }
        if options.flush {
            let mut from = Seq(0);
            while let Some((seq, len)) = self.flush_source(from, buf.len(), pdu_size) {
                let t = self
                    .transfer_mut(seq)
                    .expect("flush_source names transfers");
                let released = t.cut(len, &mut buf);
                let transfer_number = t.transfer_number;
                self.queued_bytes -= released as u64;
                // A flushed segment never ends its transfer, and a transfer
                // packed above was left no room or no bytes, so this is
                // its only entry.
                carried.push(Carried {
                    id: SendId::new(owner, LocalId::Transfer(transfer_number)),
                    completes: false,
                });
                self.settle(seq);
                from = seq.next();
            }
        }
        pad_pdu(&mut buf, padded_len);
        Some(buf.freeze())
    }

    /// What supplies the next message of a PDU holding `used` bytes, or
    /// `None` to end the PDU.
    ///
    /// Transfer Cancels go first, oldest first.  Then the first bundle in
    /// queue order that can supply its next message (see
    /// [`QueueEntry::supplies`]) does so.  A transfer that cannot, because
    /// its next segment waits on bytes not yet pushed or does not fit the
    /// room left, is passed over, interleaving transfers as Section 4.1
    /// permits; any other entry that cannot ends the PDU, so bundles are
    /// not reordered to fill it.  Emission order cannot break Section 5,
    /// because the allocator keeps every outstanding number within the
    /// window of the newest allocated.
    ///
    /// `scan` carries what earlier calls for the PDU learned.  An entry
    /// that has supplied is done with the PDU (a transfer supplies one
    /// segment to it; see [`Self::pack`]), and a transfer passed over may
    /// supply later only once the room shrinks into its
    /// [`QueuedTransfer::retry_rooms`], so each call walks those ranges
    /// and the entries not yet visited, never the whole queue.  Starved
    /// transfers are parked out of the order altogether.  This is the
    /// choice a priority scheduler would make.
    fn next_source(&self, scan: &mut Scan, used: usize, pdu_size: usize) -> Option<Source> {
        let room = pdu_size.saturating_sub(used);
        let empty = used == 0;
        if let Some(&transfer_number) = self.cancels.front() {
            return (empty || cancel_message_len(transfer_number) <= room)
                .then_some(Source::Cancel);
        }
        #[cfg(test)]
        {
            scan.probes += scan.revisit.len();
        }
        // The room only shrinks, so a range it has fallen below is spent.
        scan.revisit.retain(|(_, rooms)| room >= *rooms.start());
        if let Some(i) = scan
            .revisit
            .iter()
            .position(|(_, rooms)| rooms.contains(&room))
        {
            return Some(Source::Entry(scan.revisit.remove(i).0));
        }
        for (seq, entry) in self.order.range_from(scan.next) {
            #[cfg(test)]
            {
                scan.probes += 1;
            }
            scan.next = seq.next();
            if entry.supplies(room, empty) {
                return Some(Source::Entry(seq));
            }
            // Only a transfer is passed over.
            if let Some(rooms) = entry.as_transfer()?.retry_rooms(room) {
                scan.revisit.push((seq, rooms));
            }
        }
        None
    }

    /// The queue position, at or after `from`, of the next transfer a flush
    /// cuts into a PDU holding `used` bytes, and the data bytes it cuts
    /// (see [`QueuedTransfer::flush_chunk`]), or `None` to end the PDU.
    /// Like [`Self::next_source`], it stops at the first entry that is not
    /// a transfer, a Transfer Cancel included.  Parked transfers are
    /// visited in their queue positions.
    fn flush_source(&self, from: Seq, used: usize, pdu_size: usize) -> Option<(Seq, usize)> {
        if !self.cancels.is_empty() {
            return None;
        }
        let room = pdu_size.saturating_sub(used);
        self.queued_from(from)
            .map_while(|(seq, queued)| Some((seq, queued.as_transfer()?)))
            .find_map(|(seq, t)| Some((seq, t.flush_chunk(room, pdu_size)?)))
    }

    /// The encoded size of the messages the next PDU will pack, so a
    /// [`LinkFraming::Variable`] buffer is allocated once at the right size.
    /// It mirrors [`Self::next_source`] but only sizes the buffer: were the
    /// two to disagree, the buffer would grow or carry spare capacity, and
    /// the PDU would be the same.
    fn planned_len(&self, pdu_size: usize) -> usize {
        let mut total = 0;
        for &transfer_number in &self.cancels {
            let len = cancel_message_len(transfer_number);
            if total + len > pdu_size {
                return total;
            }
            total += len;
        }
        for entry in self.order.values() {
            let len = match entry {
                QueueEntry::Transfer(t) => {
                    // A transfer that cannot supply is passed over.
                    total += t.planned_len(pdu_size - total);
                    continue;
                }
                QueueEntry::Message { message, .. } => encoded_message_len(message),
                QueueEntry::BareBundle { .. } => return total,
            };
            if total + len > pdu_size {
                return total;
            }
            total += len;
        }
        total
    }
}

/// What supplies the next message of a PDU (see [`Sender::next_source`]).
enum Source {
    /// The oldest queued Transfer Cancel.
    Cancel,
    /// The bundle at this queue position.
    Entry(Seq),
}

/// What packing one PDU has learned of the queue (see
/// [`Sender::next_source`]).
#[derive(Default)]
struct Scan {
    /// Every entry of the order before this position has supplied to the
    /// PDU or been passed over.
    next: Seq,
    /// The transfers passed over that could still supply to the PDU, in
    /// queue order, each with the rooms it could supply to.
    revisit: Vec<(Seq, RangeInclusive<usize>)>,
    /// The queue entries and revisit ranges examined.
    #[cfg(test)]
    probes: usize,
}

/// Append a queued message to a PDU being packed.
fn encode_queued(message: &Message, buf: &mut BytesMut) {
    encode_message(message, buf).expect("queued messages are validated at enqueue");
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;
    use crate::{
        sender::{BundleFraming, LinkFraming, PduSize, SendOptions, SenderConfig},
        transfer::WindowSize,
    };

    #[test]
    fn bundles_behind_starved_transfers_are_packed_without_visiting_them() {
        let mut s = Sender::new(
            SenderConfig {
                pdu_size: PduSize::new(1500).unwrap(),
                window_size: WindowSize::MAX,
                ..SenderConfig::default()
            },
            0,
        );
        for _ in 0..1000 {
            // A dropped handle leaves its transfer queued.
            let mut h = s.begin(100_000, SendOptions::default()).unwrap();
            s.push(&mut h, Bytes::from(vec![0; 100])).unwrap();
        }
        for _ in 0..100 {
            s.enqueue(Bytes::from(vec![0; 100]), SendOptions::default())
                .unwrap();
        }

        let mut pdus = 0;
        while s.next_pdu().is_some() {
            pdus += 1;
        }

        // 14 Bundle Messages of 104 bytes to a PDU: seven full PDUs and one
        // of two.  Each PDU visits the entry it is ready with, then each
        // entry it packs; a full one also visits the message that does not
        // fit.  The last call finds the order empty.
        assert_eq!(pdus, 8);
        assert_eq!(s.probes, 7 * (1 + 14 + 1) + (1 + 2));
        assert!(s.has_pending());
    }

    #[test]
    fn oversized_queued_message_goes_out_alone_and_the_queue_moves_on() {
        const PDU: usize = 64;
        let mut s = Sender::new(
            SenderConfig {
                pdu_size: PduSize::new(PDU).unwrap(),
                link_framing: LinkFraming::variable(BundleFraming::Message),
                ..SenderConfig::default()
            },
            0,
        );
        // Break the invariant `enqueue` keeps, bypassing `message_entry`.
        let oversized = Message::Bundle {
            hints: Vec::new(),
            data: Bytes::from(vec![0; 2 * PDU]),
        };
        let oversized_len = encoded_message_len(&oversized);
        s.queue(QueueEntry::Message {
            id: LocalId::Message(100),
            message: oversized,
        });
        s.queued_bytes += 2 * PDU as u64;
        let small = s
            .enqueue(Bytes::from_static(&[1, 2, 3]), SendOptions::default())
            .unwrap();

        let pdu = s.next_pdu().unwrap();
        assert_eq!(pdu.data.len(), oversized_len);
        assert_eq!(
            &pdu.carried[..],
            &[Carried {
                id: SendId::new(s.owner, LocalId::Message(100)),
                completes: true,
            }]
        );
        let pdu = s.next_pdu().unwrap();
        assert_eq!(
            &pdu.carried[..],
            &[Carried {
                id: small,
                completes: true,
            }]
        );
        assert_eq!(s.next_pdu(), None);
    }
}
