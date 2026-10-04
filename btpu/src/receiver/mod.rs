//! The receiving end: decodes PDUs, reassembles transfers within the
//! Section 5 window under configured memory limits, and reports every
//! outcome as a [`ReceiverEvent`].

mod config;
mod event;
mod held;
mod reassembly;
mod transfer;

#[cfg(target_has_atomic = "ptr")]
use alloc::sync::Arc;
use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
use core::{fmt, num::NonZeroUsize};

use bytes::Bytes;

pub use self::{
    config::{
        Delivery, HINT_OVERHEAD, MIN_OVERHEAD_BUDGET, MaxRetainedBytes, MaxSegments,
        MaxTransferSize, ReceiverConfig, SEGMENT_OVERHEAD,
    },
    event::{DropReason, ReceiverEvent, RejectReason},
};
use self::{held::Held, reassembly::Reassembly};
#[cfg(target_has_atomic = "ptr")]
use crate::budget::RetentionBudget;
use crate::{
    codec::{BundleExtent, DecodeOptions, decode_pdu_with, message::Message},
    owner::Owner,
    transfer::{TransferId, TransferWindow},
};

/// Manages inbound PDU processing, transfer window, and segment reassembly.
///
/// # Memory
///
/// Each in-progress transfer is bounded by the [`MaxTransferSize`]: its
/// segment bytes may not exceed the cap, and its bookkeeping (segments and
/// hints) has a budget of its own; with a [`MaxSegments`] limit the segment
/// count is limited directly instead.  A segment shorter than half its PDU
/// is copied out; a longer one stays a view that keeps alive at most twice
/// its length, and is charged that, provided each PDU arrives in a buffer
/// of its own size (a `Bytes` split from a larger receive buffer pins all
/// of it).  So a transfer is charged at most three times the cap.  Up to
/// `window_size` transfers can be in progress, charged together at most the
/// [`MaxRetainedBytes`], by default one transfer's full allowance.  Charged
/// state estimates retained heap (see [`SEGMENT_OVERHEAD`]); receive
/// buffers, returned events, and allocator overhead are separate.  Closed
/// transfer numbers are remembered until the window passes them, so
/// in-progress and closed transfers together number at most the window
/// size.
///
/// # Sender restarts
///
/// A restarted sender SHOULD begin from a random transfer number (Section
/// 4).  The Figure 2 acceptance test treats a number as new only if it lies
/// within half the number space ahead of the greatest seen, so a random
/// restart lands in the accepted region with probability about one half;
/// otherwise every transfer from the new sender is reported outside the
/// window until its numbers catch up.  The draft offers no resynchronisation
/// rule.  A CLA that learns of a restart out of band should call
/// [`Self::reset`].
pub struct Receiver {
    state: Reassembly,
    /// Held apart from `state` so that [`Self::receive_pdu`] can lend it to
    /// the decoder while mutating `state`: the two are disjoint borrows, so
    /// the hook is neither shared nor taken out and put back (which a
    /// panicking hook would leave undone).  A `Box` rather than an `Arc`
    /// keeps the crate buildable on targets without pointer-width atomics.
    bundle_extent: Option<Box<dyn BundleExtent + Send + Sync>>,
}

impl fmt::Debug for Receiver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = &self.state;
        let mut f = f.debug_struct("Receiver");
        f.field("max_transfer_size", &state.max_transfer_size)
            .field("segment_limit", &state.segment_limit)
            .field("max_retained_bytes", &state.max_retained_bytes)
            .field("retained", &state.held.retained)
            .field("fec", &state.fec)
            .field("delivery", &state.delivery);
        #[cfg(target_has_atomic = "ptr")]
        f.field("budget", &state.held.budget);
        f.field("bundle_extent", &self.bundle_extent.is_some())
            .field("window", &state.window)
            .field("transfers", &state.held.len())
            .field("closed", &state.closed.len())
            .finish()
    }
}

impl Receiver {
    /// Create a new receiver.
    ///
    /// Transfers that provably exceed the configured [`MaxTransferSize`] or
    /// segment allowance, that break the FEC extension's rules, or that
    /// complete with no data are rejected with
    /// [`ReceiverEvent::TransferRejected`], and oversized or empty Bundle
    /// messages with [`ReceiverEvent::BundleRejected`].
    pub fn new(config: ReceiverConfig) -> Self {
        let default_retention = MaxRetainedBytes::for_transfers(
            NonZeroUsize::MIN,
            config.max_transfer_size,
            config.max_segments_per_transfer,
        );
        Self {
            state: Reassembly {
                max_transfer_size: config.max_transfer_size,
                segment_limit: config.max_segments_per_transfer.map(|m| u64::from(m.get())),
                max_retained_bytes: config.max_retained_bytes.unwrap_or(default_retention),
                fec: config.fec,
                delivery: config.delivery,
                owner: Owner::new(),
                window: TransferWindow::new(config.window_size),
                held: Held::default(),
                closed: BTreeMap::new(),
            },
            bundle_extent: None,
        }
    }

    /// Supply the caller's way of finding the extent of an encapsulated
    /// bundle (see [`BundleExtent`] and [`DecodeOptions::bundle_extent`]).
    ///
    /// Without one, a bare bundle frame is taken to be the whole PDU, link
    /// padding included, and a bundle found after a message ends the PDU
    /// with [`ReceiverEvent::MalformedPdu`]; see
    /// [`decode_pdu`](crate::codec::decode_pdu) for the padding pitfalls.
    pub fn with_bundle_extent(
        mut self,
        bundle_extent: impl BundleExtent + Send + Sync + 'static,
    ) -> Self {
        self.bundle_extent = Some(Box::new(bundle_extent));
        self
    }

    /// Join `budget`, a limit shared with other receivers (see
    /// [`RetentionBudget`]).  Anything already held is charged to it,
    /// whatever its limit, and released from a budget joined earlier.
    #[cfg(target_has_atomic = "ptr")]
    pub fn with_budget(mut self, budget: Arc<RetentionBudget>) -> Self {
        self.state.held.set_budget(budget);
        self
    }

    /// Give up on the transfer `id`: discard what it holds and drop its
    /// later messages as [`DropReason::Refused`].  For a CLA whose
    /// consumer refused the bundle or stopped reading it, or that stopped
    /// waiting for it.
    ///
    /// Returns whether the receiver held state for `id`.  An id whose
    /// transfer has closed or left the window is ignored, so a stale id
    /// cannot close a later transfer that reuses its number.  No event is
    /// produced.
    ///
    /// An id from another receiver is a bug in the caller: a debug build
    /// panics on it, and a release build ignores it and returns `false`.
    pub fn refuse(&mut self, id: TransferId) -> bool {
        let state = &mut self.state;
        let issued = id.owner() == state.owner;
        debug_assert!(issued, "{id:?} was issued by another Receiver");
        if !issued || state.held.get(id.key()).is_none() {
            return false;
        }
        state.close(id.key(), DropReason::Refused);
        true
    }

    /// Discard every in-progress transfer, forget closed transfer numbers,
    /// and return the window to its initial state, as if the receiver were
    /// newly constructed, except that [`TransferId`]s keep counting, so
    /// none issued before the reset names a transfer after it.
    /// Configuration and any budget are kept.  No events are produced, not
    /// even for transfers [`ReceiverEvent::TransferStarted`] reported: the
    /// caller knows what it discarded.
    pub fn reset(&mut self) {
        let state = &mut self.state;
        state.window.reset();
        state.held.clear();
        state.closed.clear();
    }

    /// The state held across all in-progress transfers, charged as
    /// [`MaxRetainedBytes`] counts it.
    ///
    /// Read between calls, it lets a CLA export a gauge and compare what
    /// its peers cost against the configured limit (see
    /// [`ReceiverConfig`] for sizing).
    pub fn retained_bytes(&self) -> usize {
        self.state.held.retained
    }

    /// The [`MaxRetainedBytes`] in effect: the configured value, or the
    /// default [`ReceiverConfig::max_retained_bytes`] derives from the other
    /// limits.
    pub fn max_retained_bytes(&self) -> MaxRetainedBytes {
        self.state.max_retained_bytes
    }

    /// Process a received convergence layer PDU.  Returns zero or more events.
    ///
    /// Infallible at the PDU level: every framing and semantic fault is
    /// expressed as an event ([`ReceiverEvent::MalformedMessage`],
    /// [`ReceiverEvent::MalformedPdu`], [`ReceiverEvent::MessageDropped`],
    /// ...) alongside whatever the well-formed messages produced, so a
    /// fault late in a PDU never discards the events of the prefix before
    /// it.
    ///
    /// Taking `pdu` by value (rather than `&[u8]`) lets the codec extract
    /// message data as zero-copy [`Bytes`] views into the original buffer.
    /// A `Bytes` made from a fresh `Vec` allocates its shared header the
    /// first time it is sliced; with [`Self::receive_pdu_into`] that is the
    /// only allocation a PDU of whole Bundle messages costs.
    ///
    /// The list grows with the number of messages in the PDU, which the
    /// peer chooses: most messages can produce an event, and one that
    /// advances the window also reports each transfer it expires.  A PDU
    /// of 8-byte Transfer Cancels for unknown transfers yields one
    /// [`ReceiverEvent::MessageDropped`] per 8 bytes, a list several times
    /// the PDU's size.  [`Self::receive_pdu_into`] fills a list the caller
    /// keeps instead, so a CLA reusing one list pays for that growth once
    /// rather than on every PDU.
    #[must_use = "events, delivered bundles included, are not reported again"]
    pub fn receive_pdu(&mut self, pdu: Bytes) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        self.receive_pdu_into(pdu, &mut events);
        events
    }

    /// [`Self::receive_pdu`], writing the events into `events` rather than
    /// a new list.
    ///
    /// `events` is cleared first, so after the call it holds exactly this
    /// PDU's events; its capacity is kept, so a list reused across PDUs is
    /// reallocated only when a PDU produces more events than any before it.
    pub fn receive_pdu_into(&mut self, pdu: Bytes, events: &mut Vec<ReceiverEvent>) {
        events.clear();
        let pdu_len = pdu.len();
        // The decoder borrows the hook for the whole PDU while `state` is
        // mutated; the fields are disjoint, so both borrows hold at once.
        let Self {
            state,
            bundle_extent,
        } = self;
        let options = DecodeOptions {
            fec: state.fec,
            bundle_extent: bundle_extent.as_deref().map(|e| e as &dyn BundleExtent),
        };
        let mut messages = decode_pdu_with(pdu, options);
        while let Some(item) = messages.next() {
            match item {
                Ok(msg) => state.process_into(msg, Some(pdu_len), events),
                Err(error) if messages.is_exhausted() => {
                    events.push(ReceiverEvent::MalformedPdu { error });
                }
                Err(error) => {
                    events.push(ReceiverEvent::MalformedMessage { error });
                }
            }
        }
    }

    /// Process a single decoded message.
    ///
    /// A message given here is not associated with a PDU, so its segment
    /// data is stored as supplied rather than copied (see [`Receiver`] for
    /// the copy rule `receive_pdu` applies).
    pub fn process_message(&mut self, message: Message) -> Vec<ReceiverEvent> {
        let mut events = Vec::new();
        self.state.process_into(message, None, &mut events);
        events
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use bytes::BytesMut;

    use super::{
        transfer::{InProgressTransfer, TransferKind, hint_charge, retain_segment},
        *,
    };
    use crate::{
        codec::{
            encode_message,
            hint::{HintItem, HintType, HintValue, Hints},
            message::TransferSegmentMessage,
        },
        fec::PreAgreedFecMessage,
        transfer::WindowSize,
    };

    fn receiver(window_size: u16, max_transfer_size: usize) -> Receiver {
        Receiver::new(ReceiverConfig {
            window_size: WindowSize::try_from(window_size).unwrap(),
            max_transfer_size: MaxTransferSize::try_from(max_transfer_size).unwrap(),
            max_segments_per_transfer: None,
            max_retained_bytes: None,
            fec: false,
            delivery: Delivery::Whole,
        })
    }

    fn segment(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
        Message::TransferSegment(TransferSegmentMessage {
            transfer_number,
            segment_index,
            hints: vec![],
            data: Bytes::from_static(data),
        })
    }

    fn end(transfer_number: u32, segment_index: u32, data: &'static [u8]) -> Message {
        Message::TransferEnd(TransferSegmentMessage {
            transfer_number,
            segment_index,
            hints: vec![],
            data: Bytes::from_static(data),
        })
    }

    #[test]
    fn closed_map_pruned_by_window_advance() {
        let mut r = receiver(4, usize::MAX);
        r.process_message(segment(0, 0, b"x"));
        r.process_message(Message::TransferCancel { transfer_number: 0 });
        assert_eq!(r.state.closed_reason(0), Some(&DropReason::Cancelled));

        // Advance the window until 0 falls out of it.
        for t in 1..=4u32 {
            r.process_message(segment(t, 0, b"x"));
        }
        assert!(r.state.closed.is_empty());

        // A really late segment for 0 is now dropped as out-of-window.
        let events = r.process_message(segment(0, 0, b"x"));
        assert_eq!(
            events,
            vec![ReceiverEvent::MessageDropped {
                transfer_number: 0,
                id: None,
                reason: DropReason::OutsideWindow,
            }]
        );
        assert!(r.state.transfer(0).is_none());
    }

    #[test]
    fn closed_map_pruned_across_the_wrap() {
        let mut r = receiver(4, usize::MAX);
        for t in [u32::MAX - 1, u32::MAX, 0] {
            r.process_message(end(t, 0, b"x"));
        }
        assert_eq!(r.state.closed.len(), 3);

        // 1 is new and leaves MAX - 1 three behind the next number; 4 moves
        // the window past everything delivered.
        r.process_message(segment(1, 0, b"x"));
        assert_eq!(r.state.closed.len(), 3);
        r.process_message(segment(4, 0, b"x"));
        assert!(r.state.closed.is_empty());
    }

    fn unknown(hint_type: u8, value: &'static [u8]) -> HintItem {
        HintItem::Unknown {
            hint_type: HintType::new(hint_type).unwrap(),
            value: HintValue::new(Bytes::from_static(value)).unwrap(),
        }
    }

    fn segment_with(
        transfer_number: u32,
        segment_index: u32,
        hints: Vec<HintItem>,
        data: &'static [u8],
    ) -> Message {
        Message::TransferSegment(TransferSegmentMessage {
            transfer_number,
            segment_index,
            hints,
            data: Bytes::from_static(data),
        })
    }

    #[test]
    fn segments_and_hints_are_charged_to_overhead_not_data() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(segment(0, 0, b"a"));
        r.process_message(segment(0, 1, b""));
        r.process_message(segment_with(
            0,
            2,
            vec![HintItem::BundleLength(3), unknown(0x41, b"corr")],
            b"b",
        ));
        let t = r.state.transfer(0).unwrap();
        assert_eq!(t.segments.len(), 3);
        assert_eq!(t.data_bytes, 2);
        assert_eq!(t.held_bytes, 2);
        // The Bundle Length is held inline and charged nothing.
        assert_eq!(t.hints.bundle_length(), Some(3));
        assert_eq!(t.hints_charge, HINT_OVERHEAD + 4);

        // Superseding a hint releases the old value's charge.
        r.process_message(segment_with(0, 3, vec![unknown(0x41, b"c")], b"c"));
        assert_eq!(r.state.transfer(0).unwrap().hints_charge, HINT_OVERHEAD + 1);

        // And a larger value raises it again.
        r.process_message(segment_with(0, 4, vec![unknown(0x41, b"longer")], b"d"));
        let t = r.state.transfer(0).unwrap();
        assert_eq!(t.hints_charge, HINT_OVERHEAD + 6);
        assert_eq!(
            r.retained_bytes(),
            t.held_bytes + 5 * SEGMENT_OVERHEAD + HINT_OVERHEAD + 6
        );
    }

    #[test]
    fn malformed_bundle_length_is_charged_and_bundle_length_is_not() {
        let hints = Hints::from(vec![HintItem::BundleLength(10), unknown(0, b"abc")]);
        assert_eq!(hint_charge(&hints), HINT_OVERHEAD + 3);
        let hints = Hints::from(vec![unknown(0, b"abc"), HintItem::BundleLength(10)]);
        assert_eq!(hint_charge(&hints), 0);
    }

    #[test]
    fn retained_segment_is_charged_what_it_keeps_alive() {
        let pdu = Bytes::from(vec![0; 100]);
        // At least half the PDU: kept as a view, charged the whole PDU.
        let (data, held) = retain_segment(pdu.slice(..50), Some(pdu.len()));
        assert_eq!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 100);
        // Shorter: copied, charged its own length.
        let (data, held) = retain_segment(pdu.slice(..49), Some(pdu.len()));
        assert_ne!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 49);
        // An odd PDU length rounds the half up, so a view never keeps alive
        // more than twice its length.
        let (data, held) = retain_segment(pdu.slice(..50), Some(101));
        assert_ne!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 50);
        // Not from a PDU: stored as given.
        let (data, held) = retain_segment(pdu.slice(..10), None);
        assert_eq!(data.as_ptr(), pdu.as_ptr());
        assert_eq!(held, 10);
    }

    #[test]
    fn duplicate_segment_is_not_charged_twice() {
        let mut r = Receiver::new(ReceiverConfig::default());
        r.process_message(segment(0, 0, b"abc"));
        r.process_message(segment(0, 0, b"abc"));
        let t = r.state.transfer(0).unwrap();
        assert_eq!(t.data_bytes, 3);
        assert_eq!(t.held_bytes, 3);
        assert_eq!(r.retained_bytes(), 3 + SEGMENT_OVERHEAD);
    }

    #[test]
    fn segment_over_the_limit_is_counted_but_never_stored() {
        let mut t = InProgressTransfer::new(TransferKind::Core);
        t.insert_segment(0, Bytes::from_static(b"ab"), None, true, Some(1));
        t.insert_segment(0, Bytes::from_static(b"ab"), None, true, Some(1));
        assert!(!t.over_segment_limit, "a duplicate is not a new segment");

        t.insert_segment(1, Bytes::from_static(b"cde"), None, true, Some(1));
        assert!(t.over_segment_limit);
        assert_eq!(t.segments.len(), 1);
        assert_eq!(t.data_bytes, 5);
        assert_eq!(t.held_bytes, 2);
    }

    #[test]
    fn reset_clears_all_state() {
        let mut r = receiver(4, usize::MAX);
        r.process_message(segment(7, 0, b"x"));
        r.process_message(segment(8, 0, b"x"));
        r.process_message(Message::TransferCancel { transfer_number: 8 });
        assert_eq!(r.state.window.greatest(), Some(8));

        r.reset();
        assert_eq!(r.state.held.len(), 0);
        assert_eq!(r.retained_bytes(), 0);
        assert!(r.state.closed.is_empty());
        assert_eq!(r.state.window.greatest(), None);
    }

    /// A xorshift generator for the randomised ledger test.  Not
    /// cryptographic, and need not be: it picks messages, not secrets, and
    /// a fixed seed keeps the test deterministic.
    struct XorShift(u64);

    impl XorShift {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// Feeds 20,000 random messages to `r`, checking after each that its
    /// retained total is the sum of its transfers' charges, and that a
    /// budget it has joined holds exactly that total.  Returns how many
    /// transfers the budget refused.
    #[cfg(target_has_atomic = "ptr")]
    fn check_ledger(mut r: Receiver, budget: &RetentionBudget) -> usize {
        let mut budget_full = 0;
        static DATA: [u8; 64] = [0xAB; 64];
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for step in 0..20_000 {
            // The numbers drift upward, so the window keeps advancing and
            // transfers keep opening rather than all closing for good.
            let transfer_number = step / 64 + rng.below(6) as u32;
            let hints = match rng.below(4) {
                0 => vec![HintItem::BundleLength(rng.below(400))],
                1 => vec![unknown(rng.below(3) as u8, &DATA[..rng.below(9) as usize])],
                _ => vec![],
            };
            let m = TransferSegmentMessage {
                transfer_number,
                segment_index: rng.below(6) as u32,
                hints,
                data: Bytes::from_static(&DATA[..rng.below(65) as usize]),
            };
            let message = match rng.below(9) {
                0..=3 => Message::TransferSegment(m),
                4 | 5 => Message::TransferEnd(m),
                6 => Message::TransferCancel { transfer_number },
                _ => Message::PreAgreedFecSource(PreAgreedFecMessage {
                    transfer_number,
                    fec_instance_id: rng.below(2) as u8,
                    hints: m.hints,
                    payload: m.data,
                }),
            };
            // Half the messages arrive in a PDU of their own, so segments
            // are both kept as views and copied.
            let events = if rng.below(2) == 0 {
                let mut pdu = BytesMut::new();
                encode_message(&message, &mut pdu).unwrap();
                r.receive_pdu(pdu.freeze())
            } else {
                r.process_message(message)
            };
            budget_full += events
                .iter()
                .filter(|e| {
                    matches!(
                        e,
                        ReceiverEvent::TransferRejected {
                            reason: RejectReason::BudgetFull,
                            ..
                        }
                    )
                })
                .count();
            let held = &r.state.held;
            assert_eq!(
                held.retained,
                held.transfers
                    .values()
                    .map(InProgressTransfer::charge)
                    .sum::<usize>(),
                "after step {step}"
            );
            assert_eq!(held.unbudgeted, 0, "after step {step}");
            assert_eq!(budget.used(), held.retained, "after step {step}");
        }
        drop(r);
        assert_eq!(budget.used(), 0, "dropping the receiver releases its share");
        budget_full
    }

    #[cfg(target_has_atomic = "ptr")]
    fn ledger_config(delivery: Delivery) -> ReceiverConfig {
        ReceiverConfig {
            window_size: WindowSize::MIN,
            max_transfer_size: MaxTransferSize::try_from(256).unwrap(),
            max_segments_per_transfer: None,
            max_retained_bytes: MaxRetainedBytes::new(2048),
            fec: true,
            delivery,
        }
    }

    #[cfg(target_has_atomic = "ptr")]
    #[test]
    fn retained_always_equals_the_sum_of_held_charges() {
        for delivery in [Delivery::Whole, Delivery::Streamed] {
            let budget = Arc::new(RetentionBudget::new(MaxRetainedBytes::MAX));
            let r = Receiver::new(ledger_config(delivery)).with_budget(Arc::clone(&budget));
            assert_eq!(check_ledger(r, &budget), 0, "{delivery:?}");
        }
    }

    #[cfg(target_has_atomic = "ptr")]
    #[test]
    fn a_budget_below_the_receiver_limit_tracks_it_exactly() {
        for delivery in [Delivery::Whole, Delivery::Streamed] {
            let budget = Arc::new(RetentionBudget::new(MaxRetainedBytes::new(300).unwrap()));
            let r = Receiver::new(ledger_config(delivery)).with_budget(Arc::clone(&budget));
            assert_ne!(
                check_ledger(r, &budget),
                0,
                "{delivery:?}: the budget refused nothing"
            );
        }
    }
}
