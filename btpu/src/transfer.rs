//! Transfer numbers and the Section 5 transfer window: the receiver's
//! acceptance test and the sender's number allocation.

use alloc::collections::VecDeque;
use core::{fmt, num::NonZeroU16};

use crate::owner::Owner;

/// Shorthand for results whose error is [`enum@Error`] unless stated.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors from transfer number allocation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The sender's transfer window is full: the next transfer number would
    /// push the oldest outstanding transfer out of the window.
    #[error("Transfer window full (size {window_size})")]
    WindowFull {
        /// The configured window size.
        window_size: WindowSize,
    },
}

/// A validated transfer window size (Section 5: 4..=4095).
///
/// Construct via [`WindowSize::new`] or [`TryFrom<u16>`], which enforce the
/// range invariant at the edge; every consumer of a `WindowSize` can then
/// rely on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "u16", into = "u16")
)]
pub struct WindowSize(NonZeroU16);

impl WindowSize {
    /// What the value configures, as error messages name it.
    const NAME: &str = "window size";

    /// Minimum allowed transfer window size (Section 5).
    pub const MIN: Self = Self(NonZeroU16::new(4).unwrap());

    /// Maximum allowed transfer window size (Section 5: less than 2^12).
    pub const MAX: Self = Self(NonZeroU16::new(4095).unwrap());

    /// The RECOMMENDED window size (Section 5), 16.  The draft marks the
    /// value as provisional ("needs discussing by the WG"), so it may
    /// change in a later revision.
    pub const DEFAULT: Self = Self(NonZeroU16::new(16).unwrap());

    /// Returns the window size for `transfers`, or `None` if it is outside
    /// [`MIN`](Self::MIN)..=[`MAX`](Self::MAX).
    pub const fn new(transfers: u16) -> Option<Self> {
        match NonZeroU16::new(transfers) {
            Some(n) if transfers >= Self::MIN.get() && transfers <= Self::MAX.get() => {
                Some(Self(n))
            }
            _ => None,
        }
    }

    /// Returns the window size as a plain integer.
    pub const fn get(self) -> u16 {
        self.0.get()
    }
}

impl Default for WindowSize {
    fn default() -> Self {
        Self::DEFAULT
    }
}

config_newtype!(WindowSize: u16);

/// A transfer admitted by a receive window, as events name it.
///
/// Transfer numbers wrap at 2³², so a number alone can name two transfers
/// a receiver saw at different times.  An id extends the number to a
/// 64-bit serial that keeps counting where the `u32` wraps and across
/// [`Receiver::reset`](crate::receiver::Receiver::reset), so within one
/// [`Receiver`](crate::receiver::Receiver) an id names exactly one
/// transfer for the receiver's lifetime: a kept id never names a later
/// transfer that reuses its number.  Ids from one receiver order its
/// transfers oldest first.
///
/// Only the receiver's window makes ids, and only for numbers inside it;
/// there is no public constructor.  Each id also carries a tag naming the
/// receiver that issued it, unique among the receivers of the process, so
/// ids from two receivers never compare equal, and
/// [`Receiver::refuse`](crate::receiver::Receiver::refuse) recognises an
/// id from another receiver as a caller bug rather than acting on a
/// transfer of its own with the same serial.
///
/// An id is 16 bytes: the serial and the tag.  The receiver keeps its own
/// state by serial alone, so the tag costs nothing per held transfer.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransferId {
    /// The receiver that issued the id.
    owner: Owner,
    key: TransferKey,
}

const _: () = assert!(size_of::<TransferId>() == 16);

impl TransferId {
    pub(crate) const fn new(owner: Owner, key: TransferKey) -> Self {
        Self { owner, key }
    }

    /// The transfer number as it appears on the wire.
    pub fn transfer_number(self) -> u32 {
        self.key.transfer_number()
    }

    pub(crate) const fn owner(self) -> Owner {
        self.owner
    }

    pub(crate) const fn key(self) -> TransferKey {
        self.key
    }
}

/// Shows the transfer number, which is what a reader can match against
/// the wire; the serial's high bits only order ids within one receiver,
/// and the receiver's tag is left out.
impl fmt::Debug for TransferId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TransferId")
            .field(&self.transfer_number())
            .finish()
    }
}

/// A transfer's serial within one window: what the receiver keys its state
/// by, and the part of a [`TransferId`] that orders it.  The serial's low
/// 32 bits are the transfer number itself, so a key is 8 bytes, which
/// matters on a constrained receiver holding up to the window size of them
/// across its in-progress and closed transfers.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct TransferKey {
    serial: u64,
}

const _: () = assert!(size_of::<TransferKey>() == 8);

impl TransferKey {
    /// The transfer number as it appears on the wire.
    pub fn transfer_number(self) -> u32 {
        // Truncation is the point: the low 32 bits are the wire number.
        self.serial as u32
    }
}

impl fmt::Debug for TransferKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("TransferKey")
            .field(&self.transfer_number())
            .finish()
    }
}

/// The outcome of [`TransferWindow::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Ahead of everything seen; the window advanced to it.
    New(TransferKey),
    /// Inside the window.
    InProgress(TransferKey),
    /// Outside the window; to be ignored.
    OutsideWindow,
}

/// Receiver-side sliding transfer window.
///
/// Implements the algorithm from Section 5 (Figure 2) of draft-ietf-dtn-btpu.
///
/// What the acceptance test makes of a restarted sender is documented on
/// [`Receiver`](crate::receiver::Receiver) under Sender restarts.
#[derive(Debug, Clone)]
pub(crate) struct TransferWindow {
    greatest: Option<TransferKey>,
    window_size: WindowSize,
    /// The serial's high 32 bits for the first number admitted while
    /// `greatest` is `None`.  [`Self::reset`] moves it past every serial
    /// issued, so ids stay unique across resets.
    epoch: u64,
}

impl TransferWindow {
    /// Create a new transfer window.
    pub fn new(window_size: WindowSize) -> Self {
        Self {
            greatest: None,
            window_size,
            // Not zero, so that stepping back a window from any first
            // number stays positive.
            epoch: 1,
        }
    }

    /// Forget the greatest transfer number seen: the next transfer number
    /// received is accepted as new whatever its value.  Ids keep counting
    /// rather than restarting, so no id issued before the reset names a
    /// transfer admitted after it.
    pub fn reset(&mut self) {
        if let Some(g) = self.greatest.take() {
            // Two epochs on, not one: the new first number's window
            // reaches up to 4095 serials below it, which must stay above
            // `g`.  An epoch per reset (plus one per 2^32 transfers)
            // cannot exhaust 2^32 epochs in practice.
            self.epoch = (g.serial >> 32) + 2;
        }
    }

    /// Classify a received transfer number, advancing the window if it is
    /// new, and return its id unless it is outside the window.  After a
    /// [`Admission::New`] the caller expires the transfers now outside the
    /// window (see [`Self::is_expired`]).
    ///
    /// The window's classification decides which way a number is extended
    /// to its serial: forward for a new number, which Section 5 accepts up
    /// to 2³¹ + W/2 ahead, beyond the half-space where serial-number
    /// comparison alone is defined; backward, by less than W, for one in
    /// progress.
    pub fn admit(&mut self, t: u32) -> Admission {
        if self.is_new_transfer(t) {
            let serial = match self.greatest {
                Some(g) => g.serial + u64::from(t.wrapping_sub(g.transfer_number())),
                None => (self.epoch << 32) + u64::from(t),
            };
            let id = TransferKey { serial };
            self.greatest = Some(id);
            Admission::New(id)
        } else if let Some(id) = self.id(t) {
            Admission::InProgress(id)
        } else {
            Admission::OutsideWindow
        }
    }

    /// The id of `t` if it is an in-progress transfer number: within the
    /// window below the greatest seen, roll-over included.  Section 5
    /// defines the set of in-progress transfers by this range alone,
    /// whether or not any message for `t` has arrived.  Never advances the
    /// window.
    ///
    /// From the Section 5 Figure 2 pseudocode:
    /// ```text
    /// RETURN ((GREATEST - T + 2^32) MOD 2^32) < WINDOW_SIZE
    /// ```
    pub fn id(&self, t: u32) -> Option<TransferKey> {
        let g = self.greatest?;
        let behind = g.transfer_number().wrapping_sub(t);
        (behind < u32::from(self.window_size.get())).then(|| TransferKey {
            serial: g.serial - u64::from(behind),
        })
    }

    /// Whether the window has moved past `id`.  Ids order oldest first,
    /// so the expired ids of a map are the leading run for which this
    /// holds.
    pub fn is_expired(&self, id: TransferKey) -> bool {
        self.greatest
            .is_some_and(|g| id.serial + u64::from(self.window_size.get()) <= g.serial)
    }

    /// Whether `id` is not ahead of the greatest number seen: true of
    /// every id the window has handed out, unless it has been reset
    /// since and admitted nothing.
    pub fn is_behind_greatest(&self, id: TransferKey) -> bool {
        self.greatest.is_some_and(|g| id <= g)
    }

    /// Returns the greatest transfer number seen so far, if any.
    #[cfg(test)]
    pub fn greatest(&self) -> Option<u32> {
        self.greatest.map(TransferKey::transfer_number)
    }

    /// Returns the configured window size.
    pub fn window_size(&self) -> WindowSize {
        self.window_size
    }

    /// Check if `t` is a "new" transfer (greater than anything seen).
    ///
    /// From the Section 5 Figure 2 pseudocode:
    /// ```text
    /// IF T = GREATEST THEN RETURN FALSE
    /// RETURN ((T - GREATEST + 2^32) MOD 2^32) < (2^32 / 2) + (WINDOW_SIZE / 2)
    /// ```
    /// The first line is the `diff != 0` guard: a repeated message for the
    /// greatest transfer is in progress, not new, so it must not re-trigger
    /// window expiry.  `WINDOW_SIZE / 2` is read as integer division, so an
    /// odd window size rounds the margin down; the draft does not say.
    fn is_new_transfer(&self, t: u32) -> bool {
        match self.greatest {
            None => true,
            Some(g) => {
                let diff = t.wrapping_sub(g.transfer_number());
                let half_space = u32::MAX / 2 + 1; // 2^31
                let half_window = u32::from(self.window_size.get()) / 2;
                diff != 0 && diff < half_space + half_window
            }
        }
    }
}

/// Allocates monotonically increasing transfer numbers for the sender.
///
/// Enforces the sender half of the Section 5 window rule: no emitted message
/// may carry a transfer number less than or equal to the greatest emitted
/// minus the window size.  Since numbers are allocated sequentially, this is
/// a bound on the *span* of outstanding numbers, not their count: the next
/// number is refused while it would push the oldest outstanding transfer out
/// of the window, even if slots have been released out of order.  Keeping
/// every outstanding transfer in-window is what lets a late or reordered
/// message for it still land inside the receiver's window.
#[derive(Debug, Clone)]
pub(crate) struct TransferNumberAllocator {
    next: u32,
    window_size: WindowSize,
    /// Outstanding transfer numbers in allocation order; the front is the
    /// oldest and anchors the window.  Kept as a sequence rather than an
    /// ordered set because allocation order, not numeric order, is what
    /// survives the modulo 2^32 roll-over.
    active: VecDeque<u32>,
}

impl TransferNumberAllocator {
    /// Create a new allocator that will allocate `initial_transfer_number`
    /// first, then increment from there.
    ///
    /// The BTP-U spec recommends choosing this value unpredictably (typically
    /// from a random source) to reduce the likelihood of a receiver mistaking
    /// the new sender for an old one after a restart.
    pub fn new(window_size: WindowSize, initial_transfer_number: u32) -> Self {
        Self {
            next: initial_transfer_number,
            window_size,
            active: VecDeque::new(),
        }
    }

    /// Whether [`Self::allocate`] would currently succeed.
    ///
    /// The next number is allocatable only if every outstanding transfer
    /// stays within the window once it becomes the greatest, i.e. while the
    /// oldest outstanding number is fewer than `window_size` numbers behind
    /// it (modulo 2^32).
    pub fn can_allocate(&self) -> bool {
        match self.active.front() {
            None => true,
            Some(&oldest) => self.next.wrapping_sub(oldest) < u32::from(self.window_size.get()),
        }
    }

    /// Allocate the next transfer number.
    ///
    /// Returns [`Error::WindowFull`] if allocating it would push the oldest
    /// outstanding transfer out of the window (see [`Self::can_allocate`]).
    pub fn allocate(&mut self) -> Result<u32> {
        if !self.can_allocate() {
            return Err(Error::WindowFull {
                window_size: self.window_size,
            });
        }
        let t = self.next;
        self.next = self.next.wrapping_add(1);
        self.active.push_back(t);
        Ok(t)
    }

    /// Release a completed or cancelled transfer.
    ///
    /// Returns `true` if `transfer_number` was outstanding and has been
    /// released, `false` if it was never allocated or was already released;
    /// a `false` release changes nothing.  Only releasing the oldest
    /// outstanding transfer lets the window advance.
    ///
    /// Costs a scan of the outstanding transfer numbers, at most the window
    /// size.
    pub fn release(&mut self, transfer_number: u32) -> bool {
        match self.active.iter().position(|&t| t == transfer_number) {
            Some(i) => {
                self.active.remove(i);
                true
            }
            None => false,
        }
    }

    /// Whether `transfer_number` is outstanding: allocated and not yet
    /// released.  Costs a scan of the outstanding transfer numbers, at most
    /// the window size.
    pub fn is_outstanding(&self, transfer_number: u32) -> bool {
        self.active.contains(&transfer_number)
    }

    /// Returns the number of transfers currently in progress.
    pub fn in_progress(&self) -> usize {
        self.active.len()
    }

    /// Returns the configured window size.
    pub fn window_size(&self) -> WindowSize {
        self.window_size
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    fn ws(transfers: u16) -> WindowSize {
        WindowSize::new(transfers).unwrap()
    }

    fn window(transfers: u16) -> TransferWindow {
        TransferWindow::new(ws(transfers))
    }

    /// An [`Admission`] without its id, for tests of the classification.
    #[derive(Debug, PartialEq)]
    enum Seen {
        New,
        InProgress,
        Outside,
    }

    fn seen(w: &mut TransferWindow, t: u32) -> Seen {
        match w.admit(t) {
            Admission::New(_) => Seen::New,
            Admission::InProgress(_) => Seen::InProgress,
            Admission::OutsideWindow => Seen::Outside,
        }
    }

    #[test]
    fn window_and_allocator_report_their_size() {
        assert_eq!(window(7).window_size(), ws(7));
        assert_eq!(TransferNumberAllocator::new(ws(7), 0).window_size(), ws(7));
    }

    #[test]
    fn first_transfer_is_new() {
        let mut w = window(16);
        assert_eq!(seen(&mut w, 100), Seen::New);
        assert_eq!(w.greatest(), Some(100));
    }

    #[test]
    fn same_transfer_is_in_progress() {
        let mut w = window(16);
        assert_eq!(seen(&mut w, 100), Seen::New);
        assert_eq!(seen(&mut w, 100), Seen::InProgress);
    }

    #[test]
    fn sequential_transfers_advance() {
        let mut w = window(4);
        for i in 0..10u32 {
            assert_eq!(seen(&mut w, i), Seen::New);
        }
        assert_eq!(w.greatest(), Some(9));
    }

    #[test]
    fn old_transfer_outside_window() {
        let mut w = window(4);
        for i in 0..10u32 {
            w.admit(i);
        }
        // greatest = 9, window = 4: valid numbers are 6..=9.
        assert_eq!(seen(&mut w, 0), Seen::Outside);
        assert_eq!(seen(&mut w, 6), Seen::InProgress);
        assert_eq!(seen(&mut w, 5), Seen::Outside);
    }

    #[test]
    fn new_transfer_boundary_is_half_space_plus_half_window() {
        // Figure 2: T is new iff (T - GREATEST) mod 2^32 < 2^31 + WINDOW_SIZE/2.
        let mut w = window(16);
        assert_eq!(seen(&mut w, 0), Seen::New);
        let boundary = (1u32 << 31) + 8;
        assert_eq!(seen(&mut w, boundary), Seen::Outside);
        assert_eq!(w.greatest(), Some(0));
        assert_eq!(seen(&mut w, boundary - 1), Seen::New);
        assert_eq!(w.greatest(), Some(boundary - 1));
    }

    #[test]
    fn odd_window_size_rounds_the_margin_down() {
        // WINDOW_SIZE / 2 is integer division: for 5 the margin is 2.
        let mut w = window(5);
        w.admit(0);
        assert_eq!(seen(&mut w, (1u32 << 31) + 2), Seen::Outside);
        assert_eq!(seen(&mut w, (1u32 << 31) + 1), Seen::New);
    }

    #[test]
    fn wraparound() {
        let mut w = window(16);
        let start = u32::MAX - 5;
        for i in 0..20u32 {
            let t = start.wrapping_add(i);
            assert_eq!(seen(&mut w, t), Seen::New, "transfer {t}");
        }
        assert_eq!(w.greatest(), Some(start.wrapping_add(19)));
    }

    #[test]
    fn expired_ids_are_those_a_full_window_behind() {
        let mut w = window(4);
        let mut ids = Vec::new();
        for t in 0..10 {
            let Admission::New(id) = w.admit(t) else {
                panic!("{t} is ahead of everything before it");
            };
            ids.push(id);
        }
        // Greatest = 9, window = 4. Valid: 6, 7, 8, 9
        let expired: Vec<u32> = ids
            .into_iter()
            .filter(|&id| w.is_expired(id))
            .map(TransferKey::transfer_number)
            .collect();
        assert_eq!(expired, vec![0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn reset_forgets_the_greatest() {
        let mut w = window(4);
        w.admit(1000);
        assert_eq!(seen(&mut w, 3), Seen::Outside);
        w.reset();
        assert_eq!(w.greatest(), None);
        assert_eq!(seen(&mut w, 3), Seen::New);
    }

    #[test]
    fn allocate_sequential() {
        let mut a = TransferNumberAllocator::new(ws(16), 100);
        assert_eq!(a.allocate(), Ok(100));
        assert_eq!(a.allocate(), Ok(101));
        assert_eq!(a.allocate(), Ok(102));
        assert_eq!(a.in_progress(), 3);
        assert!(a.is_outstanding(101));
        assert!(!a.is_outstanding(103));
    }

    #[test]
    fn release_of_oldest_frees_slot() {
        let mut a = TransferNumberAllocator::new(ws(4), 0);
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        assert!(!a.can_allocate());
        assert_eq!(a.allocate(), Err(Error::WindowFull { window_size: ws(4) }));
        assert!(a.release(0));
        assert!(a.can_allocate());
        assert_eq!(a.allocate(), Ok(4));
    }

    #[test]
    fn window_gates_on_span_not_count() {
        // Section 5: the sender MUST NOT emit a transfer number <= greatest
        // - window_size.  Releasing the newest transfer frees a *count* slot
        // but the span 0..=4 would still exceed the window while 0 is
        // outstanding.
        let mut a = TransferNumberAllocator::new(ws(4), 0);
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        assert!(a.release(3));
        assert_eq!(a.in_progress(), 3);
        assert!(!a.can_allocate());
        assert_eq!(a.allocate(), Err(Error::WindowFull { window_size: ws(4) }));

        // Releasing the oldest advances the window base to 1: 4 - 1 < 4.
        assert!(a.release(0));
        assert!(a.can_allocate());
        assert_eq!(a.allocate(), Ok(4));
        // Now 1 anchors the window: 5 - 1 == 4, refused again.
        assert!(!a.can_allocate());
    }

    #[test]
    fn span_gate_survives_wraparound() {
        let start = u32::MAX - 1;
        let mut a = TransferNumberAllocator::new(ws(4), start);
        // Allocates MAX-1, MAX, 0, 1.
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        // Numerically 1 is the smallest outstanding number, but MAX-1 is the
        // oldest and must anchor the window.
        assert!(a.release(1));
        assert!(!a.can_allocate());
        assert!(a.release(start));
        assert_eq!(a.allocate(), Ok(2));
    }

    #[test]
    fn release_of_unknown_number_is_ignored() {
        let mut a = TransferNumberAllocator::new(ws(4), 0);
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        assert!(!a.release(999));
        assert_eq!(a.in_progress(), 4);
        assert!(!a.can_allocate());
        // A repeated release of an already-released number frees nothing.
        assert!(a.release(0));
        assert!(!a.release(0));
        assert_eq!(a.in_progress(), 3);
    }

    #[test]
    fn allocator_wraps() {
        let mut a = TransferNumberAllocator::new(ws(4), u32::MAX - 1);
        assert_eq!(a.allocate(), Ok(u32::MAX - 1));
        assert_eq!(a.allocate(), Ok(u32::MAX));
        assert_eq!(a.allocate(), Ok(0));
        assert_eq!(a.allocate(), Ok(1));
    }

    #[test]
    fn ids_cover_exactly_the_window_behind_a_greatest_of_u32_max() {
        let mut w = window(4);
        assert_eq!(w.id(u32::MAX), None);
        let Admission::New(g) = w.admit(u32::MAX) else {
            panic!("the first number is new");
        };
        assert_eq!(w.id(u32::MAX), Some(g));
        let oldest = w.id(u32::MAX - 3).unwrap();
        assert_eq!(oldest.transfer_number(), u32::MAX - 3);
        assert!(oldest < g);
        assert_eq!(w.id(u32::MAX - 4), None);
        assert_eq!(w.id(0), None);
    }

    #[test]
    fn an_id_expires_once_the_window_is_a_full_width_past_it() {
        let mut w = window(4);
        w.admit(u32::MAX);
        let oldest = w.id(u32::MAX - 3).unwrap();
        let next = w.id(u32::MAX - 2).unwrap();
        assert!(!w.is_expired(oldest));

        let Admission::New(g) = w.admit(0) else {
            panic!("0 is one ahead of u32::MAX");
        };
        assert!(next < g);
        assert!(w.is_expired(oldest));
        assert!(!w.is_expired(next));
        assert_eq!(w.id(u32::MAX - 2), Some(next));
    }

    #[test]
    fn expiry_agrees_with_validity_across_the_wrap() {
        let mut w = window(4);
        let mut ids = Vec::new();
        for t in (u32::MAX - 8..=u32::MAX).chain(0..8) {
            let (Admission::New(id) | Admission::InProgress(id)) = w.admit(t) else {
                panic!("{t} is inside the window");
            };
            ids.push(id);
            for &id in &ids {
                assert_eq!(
                    w.is_expired(id),
                    w.id(id.transfer_number()).is_none(),
                    "{id:?}"
                );
            }
            assert!(ids.is_sorted());
        }
    }

    #[test]
    fn ids_after_a_reset_are_above_every_id_before_it() {
        // The greatest id before the reset has the highest low bits, and the
        // first number after it is 0, whose window reaches the furthest
        // back: every id in the new window is still above it.
        let mut w = window(WindowSize::MAX.get());
        w.admit(u32::MAX);
        let before = w.id(u32::MAX).unwrap();
        w.reset();
        assert_eq!(w.id(u32::MAX), None);
        let Admission::New(first) = w.admit(0) else {
            panic!("the first number after a reset is new");
        };
        let oldest = w
            .id(0u32.wrapping_sub(u32::from(WindowSize::MAX.get()) - 1))
            .unwrap();
        assert!(before < oldest);
        assert_eq!(first.transfer_number(), 0);

        // The same number again after another reset is a different id.
        w.reset();
        let Admission::New(again) = w.admit(0) else {
            panic!("the first number after a reset is new");
        };
        assert_ne!(again, first);
        assert!(first < again);
    }

    #[test]
    fn a_reset_before_any_transfer_changes_nothing() {
        let mut fresh = window(4);
        let mut reset = window(4);
        reset.reset();
        assert_eq!(fresh.admit(9), reset.admit(9));
    }
}
