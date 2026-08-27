use alloc::collections::VecDeque;
use core::{fmt, num::NonZeroU16, str::FromStr};

#[cfg(feature = "rand")]
use rand_core::{Rng, TryRng};

use crate::{OutOfRange, ParseError};

/// Shorthand for results whose error is [`enum@Error`] unless stated.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// Errors from transfer number allocation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The sender's transfer window is full: the next transfer number would
    /// push the oldest outstanding transfer out of the window.
    #[error("Transfer window full (size {window_size})")]
    WindowFull { window_size: WindowSize },
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
    /// [`WindowSize::DEFAULT`].
    fn default() -> Self {
        Self::DEFAULT
    }
}

forward_integer_fmt!(WindowSize);

impl FromStr for WindowSize {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, ParseError> {
        let v = s.parse::<u16>().map_err(|source| ParseError::Syntax {
            name: Self::NAME,
            source,
        })?;
        Ok(Self::try_from(v)?)
    }
}

impl TryFrom<u16> for WindowSize {
    type Error = OutOfRange;

    fn try_from(v: u16) -> Result<Self, OutOfRange> {
        Self::new(v).ok_or(OutOfRange {
            name: Self::NAME,
            value: u64::from(v),
            min: u64::from(Self::MIN.get()),
            max: Some(u64::from(Self::MAX.get())),
        })
    }
}

impl From<WindowSize> for u16 {
    fn from(w: WindowSize) -> u16 {
        w.get()
    }
}

/// Result of checking a transfer number against the receive window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferValidity {
    /// The transfer number is greater than any previously seen; it advances
    /// the window.
    New,
    /// The transfer number is within the current window (in progress).
    InProgress,
    /// The transfer number is outside the window and should be ignored.
    OutsideWindow,
}

/// A received transfer number placed in the receive window's order.
///
/// Transfer numbers wrap at 2³², so their `u32` order is not arrival
/// order: after a roll-over the newest numbers are the smallest.  A key
/// extends the number to a 64-bit serial that keeps counting where the
/// `u32` wraps, so ordering keys orders transfers oldest first, and the
/// ones a window advance expires are always a prefix.  Only
/// [`TransferWindow`] makes keys, and only for numbers inside its window,
/// so a map keyed by them holds nothing the window has not admitted.
///
/// The serial's low 32 bits are the transfer number itself, so a key is
/// 8 bytes, which matters on a constrained receiver holding up to twice
/// the window size of them.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct WindowKey {
    serial: u64,
}

const _: () = assert!(size_of::<WindowKey>() == 8);

impl WindowKey {
    /// The transfer number as it appears on the wire.
    pub fn transfer_number(self) -> u32 {
        // Truncation is the point: the low 32 bits are the wire number.
        self.serial as u32
    }
}

/// Shows the transfer number, which is what a reader of a receiver's
/// `Debug` output can match against the wire; the serial's high bits only
/// order keys within one receiver.
impl fmt::Debug for WindowKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("WindowKey")
            .field(&self.transfer_number())
            .finish()
    }
}

/// The outcome of [`TransferWindow::admit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Admission {
    /// Ahead of everything seen; the window advanced to it.
    New(WindowKey),
    /// Inside the window.
    InProgress(WindowKey),
    /// Outside the window; to be ignored.
    OutsideWindow,
}

/// Receiver-side sliding transfer window.
///
/// Implements the algorithm from Section 5 (Figure 2) of draft-ietf-dtn-btpu.
///
/// # Sender restarts
///
/// A restarted sender SHOULD begin from a random transfer number (Section
/// 4).  The Figure 2 acceptance test treats a number as new only if it lies
/// within half the number space ahead of the greatest seen, so a random
/// restart lands in the accepted region with probability about one half;
/// otherwise every transfer from the new sender is reported outside the
/// window until its numbers catch up.  The draft offers no resynchronisation
/// rule.  A CLA that learns of a peer restart out of band should
/// [`reset`](Self::reset) the window.
#[derive(Debug, Clone)]
pub struct TransferWindow {
    greatest: Option<WindowKey>,
    window_size: WindowSize,
}

impl TransferWindow {
    /// Create a new transfer window.
    pub fn new(window_size: WindowSize) -> Self {
        Self {
            greatest: None,
            window_size,
        }
    }

    /// Forget the greatest transfer number seen, returning the window to
    /// its initial state: the next transfer number received is accepted
    /// as new whatever its value.
    pub fn reset(&mut self) {
        self.greatest = None;
    }

    /// Process a received transfer number and return its validity.
    ///
    /// If the transfer is [`TransferValidity::New`], the window is advanced
    /// and the caller should expire any transfers that are now outside the
    /// window.
    pub fn process(&mut self, t: u32) -> TransferValidity {
        match self.admit(t) {
            Admission::New(_) => TransferValidity::New,
            Admission::InProgress(_) => TransferValidity::InProgress,
            Admission::OutsideWindow => TransferValidity::OutsideWindow,
        }
    }

    /// [`Self::process`], returning the admitted number's key.
    ///
    /// The window's classification decides which way a number is extended
    /// to its serial: forward for a new number, which Section 5 accepts up
    /// to 2³¹ + W/2 ahead, beyond the half-space where serial-number
    /// comparison alone is defined; backward, by less than W, for one in
    /// progress.
    pub(crate) fn admit(&mut self, t: u32) -> Admission {
        if self.is_new_transfer(t) {
            let serial = match self.greatest {
                Some(g) => g.serial + u64::from(t.wrapping_sub(g.transfer_number())),
                // Offset by 2^32 so that stepping back a window from any
                // first number stays positive.
                None => (1 << 32) + u64::from(t),
            };
            let key = WindowKey { serial };
            self.greatest = Some(key);
            Admission::New(key)
        } else if let Some(key) = self.key(t) {
            Admission::InProgress(key)
        } else {
            Admission::OutsideWindow
        }
    }

    /// The key of `t` if it is inside the window (see [`Self::is_valid`]).
    /// Never advances the window.
    pub(crate) fn key(&self, t: u32) -> Option<WindowKey> {
        let g = self.greatest?;
        // The Section 5 Figure 2 in-progress test (see `is_valid`).
        let behind = g.transfer_number().wrapping_sub(t);
        (behind < u32::from(self.window_size.get())).then(|| WindowKey {
            serial: g.serial - u64::from(behind),
        })
    }

    /// Whether the window has moved past `key`.  Keys order oldest first,
    /// so the expired keys of a map are the leading run for which this
    /// holds.
    pub(crate) fn is_expired(&self, key: WindowKey) -> bool {
        self.greatest
            .is_some_and(|g| key.serial + u64::from(self.window_size.get()) <= g.serial)
    }

    /// Whether `key` is not ahead of the greatest number seen: true of
    /// every key the window has handed out since its last reset.
    pub(crate) fn is_behind_greatest(&self, key: WindowKey) -> bool {
        self.greatest.is_some_and(|g| key <= g)
    }

    /// Returns the greatest transfer number seen so far, if any.
    pub fn greatest(&self) -> Option<u32> {
        self.greatest.map(WindowKey::transfer_number)
    }

    /// Returns the configured window size.
    pub fn window_size(&self) -> WindowSize {
        self.window_size
    }

    /// Returns transfer numbers that are now outside the window after the
    /// greatest was updated.  The caller should cancel these transfers.
    ///
    /// Given a set of active transfer numbers, yields those that are no
    /// longer valid.  Evaluation is lazy and borrows the window; collect the
    /// result before mutating the source collection.  Every number in
    /// `active` is tested, so a caller holding up to W live and W closed
    /// numbers pays O(W) per window advance; [`crate::receiver::Receiver`]
    /// instead keys its maps in window order and pays only for what
    /// expires.
    pub fn expired_transfers<'a>(
        &'a self,
        active: impl IntoIterator<Item = u32> + 'a,
    ) -> impl Iterator<Item = u32> + 'a {
        active.into_iter().filter(|&t| !self.is_valid(t))
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

    /// Whether `t` is an in-progress transfer number: within the window
    /// below the greatest seen, roll-over included.  Section 5 defines the
    /// set of in-progress transfers by this range alone, whether or not
    /// any message for `t` has arrived.  Never advances the window.
    ///
    /// From the Section 5 Figure 2 pseudocode:
    /// ```text
    /// RETURN ((GREATEST - T + 2^32) MOD 2^32) < WINDOW_SIZE
    /// ```
    pub fn is_valid(&self, t: u32) -> bool {
        self.key(t).is_some()
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
pub struct TransferNumberAllocator {
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
    /// the new sender for an old one after a restart. Use `try_from_rng` or
    /// `from_rng` (under the `rand` feature) for the common case of seeding
    /// from an RNG.
    /// See [`TransferWindow`] for what a receiver makes of a restart.
    pub fn new(window_size: WindowSize, initial_transfer_number: u32) -> Self {
        Self {
            next: initial_transfer_number,
            window_size,
            active: VecDeque::new(),
        }
    }

    /// Create a new allocator with the initial transfer number seeded from
    /// `rng`. Convenience wrapper over [`Self::new`].
    #[cfg(feature = "rand")]
    pub fn from_rng<R: Rng>(window_size: WindowSize, rng: &mut R) -> Self {
        Self::new(window_size, rng.next_u32())
    }

    /// Create a new allocator with the initial transfer number seeded from a
    /// fallible `rng`, such as the operating system's `rand::rngs::SysRng`.
    ///
    /// # Errors
    ///
    /// Returns the RNG's error if it cannot produce a value.
    #[cfg(feature = "rand")]
    pub fn try_from_rng<R: TryRng>(window_size: WindowSize, rng: &mut R) -> Result<Self, R::Error> {
        Ok(Self::new(window_size, rng.try_next_u32()?))
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
    use alloc::vec::Vec;

    use super::*;

    fn window(transfers: u16) -> TransferWindow {
        TransferWindow::new(WindowSize::new(transfers).unwrap())
    }

    #[test]
    fn keys_cover_exactly_the_window_behind_a_greatest_of_u32_max() {
        let mut w = window(4);
        assert_eq!(w.key(u32::MAX), None);
        let Admission::New(g) = w.admit(u32::MAX) else {
            panic!("the first number is new");
        };
        assert_eq!(w.key(u32::MAX), Some(g));
        let oldest = w.key(u32::MAX - 3).unwrap();
        assert_eq!(oldest.transfer_number(), u32::MAX - 3);
        assert!(oldest < g);
        assert_eq!(w.key(u32::MAX - 4), None);
        assert_eq!(w.key(0), None);
    }

    #[test]
    fn a_key_expires_once_the_window_is_a_full_width_past_it() {
        let mut w = window(4);
        w.admit(u32::MAX);
        let oldest = w.key(u32::MAX - 3).unwrap();
        let next = w.key(u32::MAX - 2).unwrap();
        assert!(!w.is_expired(oldest));

        let Admission::New(g) = w.admit(0) else {
            panic!("0 is one ahead of u32::MAX");
        };
        assert!(next < g);
        assert!(w.is_expired(oldest));
        assert!(!w.is_expired(next));
        assert_eq!(w.key(u32::MAX - 2), Some(next));
    }

    #[test]
    fn expiry_agrees_with_validity_across_the_wrap() {
        let mut w = window(4);
        let mut keys = Vec::new();
        for t in (u32::MAX - 8..=u32::MAX).chain(0..8) {
            let (Admission::New(key) | Admission::InProgress(key)) = w.admit(t) else {
                panic!("{t} is inside the window");
            };
            keys.push(key);
            for &k in &keys {
                assert_eq!(w.is_expired(k), !w.is_valid(k.transfer_number()), "{k:?}");
            }
            assert!(keys.is_sorted());
        }
    }
}
