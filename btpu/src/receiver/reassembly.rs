//! Reassembly state and message processing.

use alloc::{collections::BTreeMap, vec::Vec};

use super::{
    Delivery, DropReason, MaxRetainedBytes, MaxTransferSize, ReceiverEvent, RejectReason,
    held::Held,
    transfer::{CoreSegment, FecConfig, InProgressTransfer, Step, TransferKind},
};
#[cfg(doc)]
use super::{Receiver, ReceiverConfig};
use crate::{
    codec::{
        hint::{HintItem, HintType, Hints},
        message::{Message, TransferSegmentMessage},
    },
    owner::Owner,
    transfer::{Admission, TransferId, TransferKey, TransferWindow},
};

/// A [`Receiver`]'s configuration and reassembly state: everything but the
/// extent hook.
pub struct Reassembly {
    pub max_transfer_size: MaxTransferSize,
    /// [`ReceiverConfig::max_segments_per_transfer`], widened once for the
    /// comparison against a segment count.
    pub segment_limit: Option<u64>,
    /// [`ReceiverConfig::max_retained_bytes`], or its default.
    pub max_retained_bytes: MaxRetainedBytes,
    pub fec: bool,
    pub delivery: Delivery,
    /// Stamped on every [`TransferId`] this receiver reports, so that
    /// [`Receiver::refuse`] can tell another receiver's id from its own.
    pub owner: Owner,
    pub window: TransferWindow,
    pub held: Held,
    /// In-window transfers that are over, and why: delivered, cancelled by
    /// the sender, or rejected by local policy.  A message for one of them
    /// is a repeat or a straggler and must not re-open it (Section 4.2 for
    /// cancelled transfers; the same trap for the rest).  Keys are always
    /// in-window (pruned by [`Self::expire_old_transfers`]), so the map is
    /// bounded by the window size.
    pub closed: BTreeMap<TransferKey, DropReason>,
}

impl Reassembly {
    /// The id this receiver reports `key` under.
    fn id(&self, key: TransferKey) -> TransferId {
        TransferId::new(self.owner, key)
    }

    /// Process one message, appending its events to `events`.  `pdu_len` is
    /// the length of the PDU the message was decoded from, if any.
    pub fn process_into(
        &mut self,
        message: Message,
        pdu_len: Option<usize>,
        events: &mut Vec<ReceiverEvent>,
    ) {
        match message {
            Message::DefinitePadding { .. } | Message::Unknown { .. } => {}

            Message::Bundle { data, hints } => {
                // Section 8.1: the content MUST be a valid bundle, which zero
                // bytes cannot be; and the size cap applies unstored.
                if data.is_empty() || data.len() > self.max_transfer_size.get() {
                    events.push(ReceiverEvent::BundleRejected { len: data.len() });
                    return;
                }
                // Section 9.1: a Bundle Length hint is only meaningful on
                // Transfer Segment and End messages and SHOULD be ignored
                // elsewhere.
                let mut hints = Hints::from(hints);
                hints.remove(HintType::BUNDLE_LENGTH);
                events.push(ReceiverEvent::Received { data, hints });
            }

            Message::TransferSegment(m) => self.process_segment(m, false, pdu_len, events),
            Message::TransferEnd(m) => self.process_segment(m, true, pdu_len, events),

            Message::TransferCancel { transfer_number } => {
                self.process_transfer_cancel(transfer_number, events)
            }

            // FEC messages are tracked but not decoded (no FEC scheme is
            // implemented).  They are stored with their FecConfig to detect
            // mixing and configuration changes.
            Message::PreAgreedFecSource(m) | Message::PreAgreedFecRepair(m) => self
                .process_transfer_message(
                    m.transfer_number,
                    TransferKind::Fec(FecConfig::PreAgreed {
                        instance_id: m.fec_instance_id,
                    }),
                    m.hints,
                    None,
                    events,
                ),
            Message::ExplicitFecSource(m) | Message::ExplicitFecRepair(m) => self
                .process_transfer_message(
                    m.transfer_number,
                    TransferKind::Fec(FecConfig::Explicit {
                        encoding_id: m.fec_encoding_id,
                    }),
                    m.hints,
                    None,
                    events,
                ),
        }
    }

    /// [`Self::process_transfer_message`] for a Transfer Segment, or for a
    /// Transfer End if `end`.
    fn process_segment(
        &mut self,
        m: TransferSegmentMessage,
        end: bool,
        pdu_len: Option<usize>,
        events: &mut Vec<ReceiverEvent>,
    ) {
        let segment = CoreSegment {
            index: m.segment_index,
            data: m.data,
            end,
            pdu_len,
        };
        self.process_transfer_message(
            m.transfer_number,
            TransferKind::Core,
            m.hints,
            Some(segment),
            events,
        );
    }

    /// Admission check: is `transfer_number` eligible for processing at all?
    /// It must be inside the receive window and not already closed
    /// (delivered, cancelled, or rejected).
    ///
    /// Returns the transfer's id, or `None` once the message is reported
    /// dropped.  A number that advances the window expires the transfers it
    /// moves past, and their [`ReceiverEvent::TransferExpired`] events are
    /// pushed onto `events` whatever the outcome.
    fn admit(
        &mut self,
        transfer_number: u32,
        events: &mut Vec<ReceiverEvent>,
    ) -> Option<TransferKey> {
        let id = match self.window.admit(transfer_number) {
            Admission::OutsideWindow => {
                self.drop_message(transfer_number, None, DropReason::OutsideWindow, events);
                return None;
            }
            Admission::New(id) => {
                self.expire_old_transfers(events);
                id
            }
            Admission::InProgress(id) => id,
        };

        // A repeated message for a closed transfer MUST NOT re-open it
        // (Section 4.2 for cancelled; the same trap applies to delivered
        // and locally rejected transfers).  Checked after the window (the
        // traffic is still window-relevant) but before any transfer entry
        // is inserted.
        match self.closed.get(&id) {
            Some(&reason) => {
                self.drop_message(transfer_number, Some(id), reason, events);
                None
            }
            None => Some(id),
        }
    }

    /// Which limit, if any, a state change has taken the transfer at `id`
    /// or the receiver over: the transfer's [`MaxTransferSize`] rules (see
    /// [`InProgressTransfer::exceeds`]), then the receiver's
    /// [`MaxRetainedBytes`] as [`RejectReason::ReceiverFull`], then the
    /// shared budget as [`RejectReason::BudgetFull`].
    ///
    /// The receiver's total was within its limits before the change, so the
    /// transfer that grew is the one to reject.
    fn over_limit(&self, id: TransferKey) -> Option<RejectReason> {
        self.held
            .get(id)
            .and_then(|t| t.exceeds(self.max_transfer_size.get(), self.segment_limit))
            .or_else(|| {
                (self.held.retained > self.max_retained_bytes.get())
                    .then_some(RejectReason::ReceiverFull)
            })
            .or_else(|| (self.held.unbudgeted > 0).then_some(RejectReason::BudgetFull))
    }

    /// Report a message that is dropped with no state touched.  Drops are
    /// expected traffic (repetition, reordering, a moved window), not
    /// faults, so they surface as [`ReceiverEvent::MessageDropped`].
    fn drop_message(
        &self,
        transfer_number: u32,
        key: Option<TransferKey>,
        reason: DropReason,
        events: &mut Vec<ReceiverEvent>,
    ) {
        events.push(ReceiverEvent::MessageDropped {
            transfer_number,
            id: key.map(|key| self.id(key)),
            reason,
        });
    }

    /// [`Self::close`] the transfer at `id` as rejected for `reason`, and
    /// report it.
    fn reject_transfer(
        &mut self,
        id: TransferKey,
        reason: RejectReason,
        events: &mut Vec<ReceiverEvent>,
    ) {
        self.close(id, reason.into());
        events.push(ReceiverEvent::TransferRejected {
            id: self.id(id),
            reason,
        });
    }

    /// Shared pipeline for every message that opens or extends a transfer
    /// (Segment, End, and the four FEC messages): admission, transfer-kind
    /// check, sequence-conflict check, state application, oversize gate,
    /// completion check.
    ///
    /// `kind` is what this message would make a new transfer.  `segment` is
    /// the content of a Segment or End, and `None` for an FEC message,
    /// whose payload is not stored while no FEC scheme is implemented; its
    /// hints are still kept, and the Bundle Length hint still policed.
    fn process_transfer_message(
        &mut self,
        transfer_number: u32,
        kind: TransferKind,
        hints: Vec<HintItem>,
        segment: Option<CoreSegment>,
        events: &mut Vec<ReceiverEvent>,
    ) {
        let Some(id) = self.admit(transfer_number, events) else {
            return;
        };

        let transfer = self.held.get_or_open(id, kind);

        if let Some(reason) = transfer.kind_mismatch(kind) {
            return self.reject_transfer(id, reason, events);
        }

        // A conflicting or duplicate message is dropped with no state
        // touched, hints included.
        if segment.as_ref().is_some_and(|s| s.conflicts(transfer)) {
            return self.drop_message(
                transfer_number,
                Some(id),
                DropReason::SegmentIndexConflict,
                events,
            );
        }
        if segment.as_ref().is_some_and(|s| s.is_duplicate(transfer)) {
            return self.drop_message(transfer_number, Some(id), DropReason::Duplicate, events);
        }

        let segment_limit = self.segment_limit;
        let streamed = self.delivery == Delivery::Streamed;
        self.held.modify(id, |transfer| {
            transfer.apply_hints(hints);
            if let Some(CoreSegment {
                index,
                data,
                end,
                pdu_len,
            }) = segment
            {
                if end {
                    transfer.final_segment_index = Some(index);
                }
                // An empty segment or End is still stored: Section 4
                // completes a transfer once indices 0..=N are present, and a
                // streaming sender may have no other way to finish.  The
                // segment charge bounds a flood of them.  The next segment
                // of a streamed transfer is released below, before this
                // call returns, so it is neither copied nor charged against
                // the retention limits; it still counts toward the
                // transfer's own limits.
                let retain = !(streamed && transfer.is_next(index));
                transfer.insert_segment(index, data, pdu_len, retain, segment_limit);
            }
        });

        if let Some(reason) = self.over_limit(id) {
            return self.reject_transfer(id, reason, events);
        }

        // A late segment may fill the last gap after the End arrived.
        if streamed {
            self.release_prefix(id, events);
        } else {
            self.complete_if_ready(id, events);
        }
    }

    /// Under [`Delivery::Streamed`], release the transfer's segments that
    /// are now contiguous from its start, reporting
    /// [`ReceiverEvent::TransferStarted`] before the first data and
    /// [`ReceiverEvent::TransferFinished`] for the last segment, which
    /// closes the transfer.  A no-op if the next segment has not arrived,
    /// as for an FEC transfer, whose payload is not stored.
    ///
    /// The release path takes the next contiguous bytes from
    /// [`InProgressTransfer::release_next`]; an FEC scheme would feed it
    /// decoded source bytes the same way.
    fn release_prefix(&mut self, id: TransferKey, events: &mut Vec<ReceiverEvent>) {
        loop {
            let (start, data, last, hints) =
                match self.held.modify(id, InProgressTransfer::next_step) {
                    Some(Step::Out {
                        start,
                        data,
                        last,
                        hints,
                    }) => (start, data, last, hints),
                    Some(Step::Empty) => {
                        return self.reject_transfer(id, RejectReason::Empty, events);
                    }
                    Some(Step::Waiting) | None => return,
                };
            if let Some(hints) = start {
                events.push(ReceiverEvent::TransferStarted {
                    id: self.id(id),
                    hints,
                });
            }
            if last {
                // Closed rather than forgotten, as for a whole delivery.
                self.close(id, DropReason::Delivered);
                events.push(ReceiverEvent::TransferFinished {
                    id: self.id(id),
                    data,
                    hints,
                });
                return;
            }
            events.push(ReceiverEvent::TransferData {
                id: self.id(id),
                data,
                hints,
            });
        }
    }

    /// If the transfer's segments are all present (and its final index is
    /// known), reassemble it, remove it from the window, and push a
    /// `Received` event.  A no-op otherwise.  Called after every segment
    /// or End insert so out-of-order completion is detected regardless of which
    /// message arrives last.
    fn complete_if_ready(&mut self, id: TransferKey, events: &mut Vec<ReceiverEvent>) {
        let Some(transfer) = self.held.get(id) else {
            return;
        };
        if !transfer.is_complete() {
            return;
        }
        // The size limits were enforced on every insert.  Zero bytes cannot
        // be a valid bundle, as for an empty Bundle Message (Section 8.1).
        if transfer.data_bytes == 0 {
            return self.reject_transfer(id, RejectReason::Empty, events);
        }
        // Reassembled while the segments are still charged, so the copy is
        // never made against budget accounted free.
        let data = transfer.reassemble();
        // Closed rather than forgotten: a sender may repeat any message
        // (Section 6), and a repeat must not deliver the bundle again.
        let hints = self.close(id, DropReason::Delivered);
        events.push(ReceiverEvent::Received { data, hints });
    }

    fn process_transfer_cancel(&mut self, transfer_number: u32, events: &mut Vec<ReceiverEvent>) {
        // Section 8.4 ignores a Cancel for a transfer not in progress, and
        // Section 5 defines in progress by the window alone.  So a Cancel
        // never advances the window, and one inside it is recorded even
        // before any segment arrives, so later segments are discarded.
        let Some(id) = self.window.id(transfer_number) else {
            return self.drop_message(transfer_number, None, DropReason::UnknownTransfer, events);
        };

        // A repeated Cancel of a transfer already closed is idempotent and
        // reported with the original reason.
        if let Some(&reason) = self.closed.get(&id) {
            return self.drop_message(transfer_number, Some(id), reason, events);
        }

        // A closed transfer is never also in progress, so this discards the
        // transfer's segments if any have arrived, and otherwise records
        // the Cancel ahead of them.
        self.close(id, DropReason::Cancelled);
        events.push(ReceiverEvent::TransferCancelled { id: self.id(id) });
    }

    /// Drop every transfer, live or closed, that the window has moved past,
    /// reporting the live ones as [`ReceiverEvent::TransferExpired`] oldest
    /// first.  Both maps are keyed in window order, so the expired entries
    /// are a leading run and the cost is O(log n) per entry expired, not a
    /// walk of the window.
    fn expire_old_transfers(&mut self, events: &mut Vec<ReceiverEvent>) {
        debug_assert!(
            self.held
                .newest()
                .is_none_or(|k| self.window.is_behind_greatest(k))
                && self
                    .closed
                    .last_key_value()
                    .is_none_or(|(&k, _)| self.window.is_behind_greatest(k)),
            "a held transfer is ahead of the window"
        );
        while let Some(key) = self.held.pop_expired(&self.window) {
            events.push(ReceiverEvent::TransferExpired { id: self.id(key) });
        }

        // Prune the closed map the same way; this is what keeps it bounded
        // by the window size.  No events: these were already reported as
        // Received / TransferCancelled / TransferRejected when they
        // closed.
        while let Some(entry) = self.closed.first_entry()
            && self.window.is_expired(*entry.key())
        {
            entry.remove();
        }

        // Every id in either map is in the window, and no id is in both.
        debug_assert!(
            self.held.len() + self.closed.len() <= usize::from(self.window.window_size().get()),
            "more transfers remembered than the window holds"
        );
    }

    /// Remember the transfer at `id` as closed with `reason`, so later
    /// messages for it are dropped rather than re-opening it, and remove its
    /// in-progress state, if any, releasing its charge.  Returns the
    /// transfer's hints, empty if it had no state.
    pub fn close(&mut self, id: TransferKey, reason: DropReason) -> Hints {
        self.closed.insert(id, reason);
        self.held
            .remove(id)
            .map(|transfer| transfer.hints)
            .unwrap_or_default()
    }

    /// The in-progress transfer numbered `transfer_number`, looked up
    /// through the window as production code does.
    #[cfg(test)]
    pub fn transfer(&self, transfer_number: u32) -> Option<&InProgressTransfer> {
        self.held.get(self.window.id(transfer_number)?)
    }

    /// Why the transfer numbered `transfer_number` closed, if it has.
    #[cfg(test)]
    pub fn closed_reason(&self, transfer_number: u32) -> Option<&DropReason> {
        self.closed.get(&self.window.id(transfer_number)?)
    }
}
