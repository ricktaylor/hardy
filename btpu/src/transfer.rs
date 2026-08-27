use alloc::collections::VecDeque;

/// Errors from transfer window sizing and number allocation.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The window size is outside [`WindowSize::MIN`]..=[`WindowSize::MAX`].
    #[error(
        "Invalid window size {0} (must be {min}..={max})",
        min = WindowSize::MIN,
        max = WindowSize::MAX
    )]
    InvalidWindowSize(u16),

    /// The sender's transfer window is full: the next transfer number would
    /// push the oldest outstanding transfer out of the window.
    #[error("Transfer window full (size {window_size})")]
    WindowFull { window_size: u16 },
}

/// A validated transfer window size (Section 5: 4..=4095).
///
/// Construct via [`TryFrom<u16>`], which enforces the range invariant at the
/// edge; every consumer of a `WindowSize` can then rely on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(try_from = "u16", into = "u16")
)]
pub struct WindowSize(u16);

impl WindowSize {
    /// Minimum allowed transfer window size (Section 5).
    pub const MIN: u16 = 4;

    /// Maximum allowed transfer window size (Section 5: less than 2^12).
    pub const MAX: u16 = 4095;

    /// Returns the window size as a plain integer.
    pub fn get(self) -> u16 {
        self.0
    }
}

impl Default for WindowSize {
    /// The RECOMMENDED window size (Section 5).
    fn default() -> Self {
        Self(16)
    }
}

impl TryFrom<u16> for WindowSize {
    type Error = Error;

    fn try_from(v: u16) -> Result<Self, Self::Error> {
        if (Self::MIN..=Self::MAX).contains(&v) {
            Ok(Self(v))
        } else {
            Err(Error::InvalidWindowSize(v))
        }
    }
}

impl From<WindowSize> for u16 {
    fn from(w: WindowSize) -> u16 {
        w.0
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

/// Receiver-side sliding transfer window.
///
/// Implements the algorithm from Section 5 of draft-ietf-dtn-btpu.
pub struct TransferWindow {
    greatest: Option<u32>,
    window_size: u16,
}

impl TransferWindow {
    /// Create a new transfer window.
    pub fn new(window_size: WindowSize) -> Self {
        Self {
            greatest: None,
            window_size: window_size.get(),
        }
    }

    /// Process a received transfer number and return its validity.
    ///
    /// If the transfer is [`TransferValidity::New`], the window is advanced
    /// and the caller should expire any transfers that are now outside the
    /// window.
    pub fn process(&mut self, t: u32) -> TransferValidity {
        if self.is_new_transfer(t) {
            self.greatest = Some(t);
            TransferValidity::New
        } else if self.is_valid(t) {
            TransferValidity::InProgress
        } else {
            TransferValidity::OutsideWindow
        }
    }

    /// Returns the greatest transfer number seen so far, if any.
    pub fn greatest(&self) -> Option<u32> {
        self.greatest
    }

    /// Returns the configured window size.
    pub fn window_size(&self) -> u16 {
        self.window_size
    }

    /// Returns transfer numbers that are now outside the window after the
    /// greatest was updated.  The caller should cancel these transfers.
    ///
    /// Given a set of active transfer numbers, yields those that are no
    /// longer valid.  Evaluation is lazy and borrows the window; collect the
    /// result before mutating the source collection.
    pub fn expired_transfers<'a>(
        &'a self,
        active: impl IntoIterator<Item = u32> + 'a,
    ) -> impl Iterator<Item = u32> + 'a {
        active.into_iter().filter(|&t| !self.is_valid(t))
    }

    /// Check if `t` is a "new" transfer (greater than anything seen).
    ///
    /// From the spec pseudocode:
    /// ```text
    /// RETURN ((T - GREATEST + 2^32) MOD 2^32) < (2^32 / 2) + (WINDOW_SIZE / 2)
    /// ```
    fn is_new_transfer(&self, t: u32) -> bool {
        match self.greatest {
            None => true,
            Some(g) => {
                let diff = t.wrapping_sub(g);
                let half_space = u32::MAX / 2 + 1; // 2^31
                let half_window = self.window_size as u32 / 2;
                diff != 0 && diff < half_space + half_window
            }
        }
    }

    /// Check if `t` is within the valid window.
    ///
    /// From the spec pseudocode:
    /// ```text
    /// RETURN ((GREATEST - T + 2^32) MOD 2^32) < WINDOW_SIZE
    /// ```
    fn is_valid(&self, t: u32) -> bool {
        match self.greatest {
            None => false,
            Some(g) => {
                let diff = g.wrapping_sub(t);
                diff < self.window_size as u32
            }
        }
    }
}

/// Allocates monotonically increasing transfer numbers for the sender.
///
/// Enforces the sender half of the Section 5 window rule: no emitted message
/// may carry a transfer number less than or equal to the greatest emitted
/// minus the window size.  Since numbers are allocated sequentially, this is
/// a bound on the *span* of outstanding numbers, not their count — the next
/// number is refused while it would push the oldest outstanding transfer out
/// of the window, even if slots have been released out of order.  Keeping
/// every outstanding transfer in-window is what lets a late or reordered
/// message for it still land inside the receiver's window.
pub struct TransferNumberAllocator {
    next: u32,
    window_size: u16,
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
    /// the new sender for an old one after a restart. Use [`Self::from_rng`]
    /// (under the `rand` feature) for the common case of seeding from an RNG.
    pub fn new(window_size: WindowSize, initial_transfer_number: u32) -> Self {
        Self {
            next: initial_transfer_number,
            window_size: window_size.get(),
            active: VecDeque::new(),
        }
    }

    /// Create a new allocator with the initial transfer number seeded from
    /// `rng`. Convenience wrapper over [`Self::new`].
    #[cfg(feature = "rand")]
    pub fn from_rng<R: rand_core::Rng>(window_size: WindowSize, rng: &mut R) -> Self {
        Self::new(window_size, rng.next_u32())
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
            Some(&oldest) => self.next.wrapping_sub(oldest) < self.window_size as u32,
        }
    }

    /// Allocate the next transfer number.
    ///
    /// Returns [`Error::WindowFull`] if allocating it would push the oldest
    /// outstanding transfer out of the window (see [`Self::can_allocate`]).
    pub fn allocate(&mut self) -> Result<u32, Error> {
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
    pub fn release(&mut self, transfer_number: u32) -> bool {
        match self.active.iter().position(|&t| t == transfer_number) {
            Some(i) => {
                self.active.remove(i);
                true
            }
            None => false,
        }
    }

    /// Returns the number of transfers currently in progress.
    pub fn in_progress(&self) -> usize {
        self.active.len()
    }

    /// Returns the configured window size.
    pub fn window_size(&self) -> u16 {
        self.window_size
    }
}

#[cfg(test)]
mod tests {
    use alloc::{vec, vec::Vec};

    use super::*;

    fn ws(v: u16) -> WindowSize {
        WindowSize::try_from(v).unwrap()
    }

    // -- WindowSize tests -----------------------------------------------

    #[test]
    fn window_size_boundaries() {
        // Section 5: 4..=4095 (less than 2^12).
        assert_eq!(ws(WindowSize::MIN).get(), 4);
        assert_eq!(ws(WindowSize::MAX).get(), 4095);
        assert!(matches!(
            WindowSize::try_from(3),
            Err(Error::InvalidWindowSize(3))
        ));
        assert!(matches!(
            WindowSize::try_from(4096),
            Err(Error::InvalidWindowSize(4096))
        ));
    }

    #[test]
    fn window_size_default_is_recommended() {
        assert_eq!(WindowSize::default().get(), 16);
    }

    #[test]
    fn first_transfer_is_new() {
        let mut w = TransferWindow::new(ws(16));
        assert_eq!(w.process(100), TransferValidity::New);
        assert_eq!(w.greatest(), Some(100));
    }

    #[test]
    fn same_transfer_is_in_progress() {
        let mut w = TransferWindow::new(ws(16));
        assert_eq!(w.process(100), TransferValidity::New);
        assert_eq!(w.process(100), TransferValidity::InProgress);
    }

    #[test]
    fn sequential_transfers_advance() {
        let mut w = TransferWindow::new(ws(4));
        for i in 0..10u32 {
            assert_eq!(w.process(i), TransferValidity::New);
        }
        assert_eq!(w.greatest(), Some(9));
    }

    #[test]
    fn old_transfer_outside_window() {
        let mut w = TransferWindow::new(ws(4));
        for i in 0..10u32 {
            w.process(i);
        }
        // Transfer 0 is now well outside the window (greatest=9, window=4)
        assert_eq!(w.process(0), TransferValidity::OutsideWindow);
        // Transfer 6 is also outside (9 - 6 = 3, but valid requires < 4, so 6 is valid)
        assert_eq!(w.process(6), TransferValidity::InProgress);
        // Transfer 5 is outside (9 - 5 = 4, not < 4)
        assert_eq!(w.process(5), TransferValidity::OutsideWindow);
    }

    #[test]
    fn wraparound() {
        let mut w = TransferWindow::new(ws(16));
        // Start near u32::MAX
        let start = u32::MAX - 5;
        for i in 0..20u32 {
            let t = start.wrapping_add(i);
            assert_eq!(w.process(t), TransferValidity::New, "transfer {t}");
        }
        // Greatest should have wrapped around
        assert_eq!(w.greatest(), Some(start.wrapping_add(19)));
    }

    #[test]
    fn expired_transfers_detected() {
        let mut w = TransferWindow::new(ws(4));
        let active: Vec<u32> = (0..10).collect();
        for &t in &active {
            w.process(t);
        }
        // Greatest = 9, window = 4. Valid: 6, 7, 8, 9
        let expired: Vec<u32> = w.expired_transfers(active).collect();
        assert_eq!(expired, vec![0, 1, 2, 3, 4, 5]);
    }

    // -- TransferNumberAllocator tests --------------------------------------

    #[test]
    fn allocate_sequential() {
        let mut a = TransferNumberAllocator::new(ws(16), 100);
        assert_eq!(a.allocate().unwrap(), 100);
        assert_eq!(a.allocate().unwrap(), 101);
        assert_eq!(a.allocate().unwrap(), 102);
        assert_eq!(a.in_progress(), 3);
    }

    #[test]
    fn window_full() {
        let mut a = TransferNumberAllocator::new(ws(4), 0);
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        assert!(!a.can_allocate());
        assert!(matches!(
            a.allocate(),
            Err(Error::WindowFull { window_size: 4 })
        ));
    }

    #[test]
    fn release_of_oldest_frees_slot() {
        let mut a = TransferNumberAllocator::new(ws(4), 0);
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        assert!(a.allocate().is_err());
        assert!(a.release(0));
        assert!(a.can_allocate());
        assert_eq!(a.allocate().unwrap(), 4);
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
        assert!(matches!(
            a.allocate(),
            Err(Error::WindowFull { window_size: 4 })
        ));

        // Releasing the oldest advances the window base to 1: 4 - 1 < 4.
        assert!(a.release(0));
        assert!(a.can_allocate());
        assert_eq!(a.allocate().unwrap(), 4);
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
        assert_eq!(a.allocate().unwrap(), 2);
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

    #[cfg(feature = "rand")]
    #[test]
    fn allocator_from_rng_seeds_first_number() {
        struct FixedRng(u32);
        impl rand::rand_core::TryRng for FixedRng {
            type Error = core::convert::Infallible;
            fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
                Ok(self.0)
            }
            fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
                Ok(self.0 as u64)
            }
            fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
                dst.fill(0);
                Ok(())
            }
        }

        let mut a = TransferNumberAllocator::from_rng(ws(16), &mut FixedRng(12345));
        assert_eq!(a.allocate().unwrap(), 12345);
    }

    #[test]
    fn allocator_wraps() {
        let mut a = TransferNumberAllocator::new(ws(4), u32::MAX - 1);
        assert_eq!(a.allocate().unwrap(), u32::MAX - 1);
        assert_eq!(a.allocate().unwrap(), u32::MAX);
        assert_eq!(a.allocate().unwrap(), 0);
        assert_eq!(a.allocate().unwrap(), 1);
    }
}
