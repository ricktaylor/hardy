use alloc::vec::Vec;

use crate::error::Error;

/// Minimum allowed transfer window size (Section 5).
pub const MIN_WINDOW_SIZE: u16 = 4;

/// Maximum allowed transfer window size (Section 5).
pub const MAX_WINDOW_SIZE: u16 = 4096;

/// Recommended default transfer window size (Section 5).
pub const DEFAULT_WINDOW_SIZE: u16 = 16;

// ---------------------------------------------------------------------------
// Transfer validity
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Receiver-side transfer window (Section 5, Figure 2)
// ---------------------------------------------------------------------------

/// Receiver-side sliding transfer window.
///
/// Implements the algorithm from Section 5 of draft-ietf-dtn-btpu-02.
pub struct TransferWindow {
    greatest: Option<u32>,
    window_size: u16,
}

impl TransferWindow {
    /// Create a new transfer window.
    ///
    /// # Panics
    ///
    /// Panics if `window_size` is outside [`MIN_WINDOW_SIZE`]..=[`MAX_WINDOW_SIZE`].
    pub fn new(window_size: u16) -> Self {
        assert!(
            (MIN_WINDOW_SIZE..=MAX_WINDOW_SIZE).contains(&window_size),
            "window_size {window_size} out of range {MIN_WINDOW_SIZE}..={MAX_WINDOW_SIZE}"
        );
        Self {
            greatest: None,
            window_size,
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
    /// Given a set of active transfer numbers, returns those that are no
    /// longer valid.
    pub fn expired_transfers<'a>(&self, active: impl Iterator<Item = &'a u32>) -> Vec<u32> {
        active.filter(|&&t| !self.is_valid(t)).copied().collect()
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

// ---------------------------------------------------------------------------
// Sender-side transfer number allocator
// ---------------------------------------------------------------------------

/// Allocates monotonically increasing transfer numbers for the sender.
pub struct TransferNumberAllocator {
    next: u32,
    window_size: u16,
    in_progress: u32,
}

impl TransferNumberAllocator {
    /// Create a new allocator that will allocate `initial_transfer_number`
    /// first, then increment from there.
    ///
    /// The BTP-U spec recommends choosing this value unpredictably (typically
    /// from a random source) to reduce the likelihood of a receiver mistaking
    /// the new sender for an old one after a restart. Use [`Self::from_rng`]
    /// (under the `rand` feature) for the common case of seeding from an RNG.
    ///
    /// # Panics
    ///
    /// Panics if `window_size` is outside [`MIN_WINDOW_SIZE`]..=[`MAX_WINDOW_SIZE`].
    pub fn new(window_size: u16, initial_transfer_number: u32) -> Self {
        assert!(
            (MIN_WINDOW_SIZE..=MAX_WINDOW_SIZE).contains(&window_size),
            "window_size {window_size} out of range {MIN_WINDOW_SIZE}..={MAX_WINDOW_SIZE}"
        );
        Self {
            next: initial_transfer_number,
            window_size,
            in_progress: 0,
        }
    }

    /// Create a new allocator with the initial transfer number seeded from
    /// `rng`. Convenience wrapper over [`Self::new`].
    #[cfg(feature = "rand")]
    pub fn from_rng<R: rand_core::RngCore>(window_size: u16, rng: &mut R) -> Self {
        Self::new(window_size, rng.next_u32())
    }

    /// Allocate the next transfer number.
    ///
    /// Returns [`Error::WindowFull`] if the window is at capacity.
    pub fn allocate(&mut self) -> Result<u32, Error> {
        if self.in_progress >= self.window_size as u32 {
            return Err(Error::WindowFull {
                window_size: self.window_size,
            });
        }
        let t = self.next;
        self.next = self.next.wrapping_add(1);
        self.in_progress += 1;
        Ok(t)
    }

    /// Release a completed or cancelled transfer, freeing a window slot.
    pub fn release(&mut self) {
        self.in_progress = self.in_progress.saturating_sub(1);
    }

    /// Returns the number of transfers currently in progress.
    pub fn in_progress(&self) -> u32 {
        self.in_progress
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    // -- TransferWindow tests -----------------------------------------------

    #[test]
    fn first_transfer_is_new() {
        let mut w = TransferWindow::new(16);
        assert_eq!(w.process(100), TransferValidity::New);
        assert_eq!(w.greatest(), Some(100));
    }

    #[test]
    fn same_transfer_is_in_progress() {
        let mut w = TransferWindow::new(16);
        assert_eq!(w.process(100), TransferValidity::New);
        assert_eq!(w.process(100), TransferValidity::InProgress);
    }

    #[test]
    fn sequential_transfers_advance() {
        let mut w = TransferWindow::new(4);
        for i in 0..10u32 {
            assert_eq!(w.process(i), TransferValidity::New);
        }
        assert_eq!(w.greatest(), Some(9));
    }

    #[test]
    fn old_transfer_outside_window() {
        let mut w = TransferWindow::new(4);
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
        let mut w = TransferWindow::new(16);
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
        let mut w = TransferWindow::new(4);
        let active: Vec<u32> = (0..10).collect();
        for &t in &active {
            w.process(t);
        }
        // Greatest = 9, window = 4. Valid: 6, 7, 8, 9
        let expired = w.expired_transfers(active.iter());
        assert_eq!(expired, vec![0, 1, 2, 3, 4, 5]);
    }

    // -- TransferNumberAllocator tests --------------------------------------

    #[test]
    fn allocate_sequential() {
        let mut a = TransferNumberAllocator::new(16, 100);
        assert_eq!(a.allocate().unwrap(), 100);
        assert_eq!(a.allocate().unwrap(), 101);
        assert_eq!(a.allocate().unwrap(), 102);
        assert_eq!(a.in_progress(), 3);
    }

    #[test]
    fn window_full() {
        let mut a = TransferNumberAllocator::new(4, 0);
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        assert!(a.allocate().is_err());
    }

    #[test]
    fn release_frees_slot() {
        let mut a = TransferNumberAllocator::new(4, 0);
        for _ in 0..4 {
            a.allocate().unwrap();
        }
        assert!(a.allocate().is_err());
        a.release();
        assert_eq!(a.allocate().unwrap(), 4);
    }

    #[test]
    fn allocator_wraps() {
        let mut a = TransferNumberAllocator::new(4, u32::MAX - 1);
        assert_eq!(a.allocate().unwrap(), u32::MAX - 1);
        assert_eq!(a.allocate().unwrap(), u32::MAX);
        assert_eq!(a.allocate().unwrap(), 0);
        assert_eq!(a.allocate().unwrap(), 1);
    }
}
