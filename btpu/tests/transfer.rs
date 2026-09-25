//! Transfer window and transfer-number allocation through the public
//! `transfer` API.

mod common;

use hardy_btpu::{
    OutOfRange, ParseError,
    transfer::{Error, TransferNumberAllocator, TransferValidity, TransferWindow, WindowSize},
};

use self::common::window_size as ws;

#[test]
fn window_size_boundaries() {
    // Section 5: 4..=4095 (less than 2^12).
    assert_eq!(WindowSize::MIN.get(), 4);
    assert_eq!(WindowSize::MAX.get(), 4095);
    assert_eq!(WindowSize::try_from(4), Ok(WindowSize::MIN));
    assert_eq!(WindowSize::try_from(4095), Ok(WindowSize::MAX));
    for value in [0, 3, 4096] {
        assert_eq!(WindowSize::new(value), None);
        assert_eq!(
            WindowSize::try_from(value),
            Err(OutOfRange {
                name: "window size",
                value: u64::from(value),
                min: 4,
                max: Some(4095),
            })
        );
    }
}

#[test]
fn window_size_default_is_recommended() {
    assert_eq!(WindowSize::default(), WindowSize::DEFAULT);
    assert_eq!(WindowSize::DEFAULT.get(), 16);
    assert_eq!(WindowSize::DEFAULT.to_string(), "16");
}

#[test]
fn window_size_parses_and_formats_as_its_integer() {
    assert_eq!("16".parse(), Ok(WindowSize::DEFAULT));
    assert_eq!(
        "4096".parse::<WindowSize>(),
        Err(ParseError::OutOfRange(
            WindowSize::try_from(4096).unwrap_err()
        ))
    );
    // Too wide for the u16 it parses into: a syntax error, not a range one.
    assert_eq!(
        "65536".parse::<WindowSize>(),
        Err(ParseError::Syntax {
            name: "window size",
            source: "65536".parse::<u16>().unwrap_err(),
        })
    );
    let w = WindowSize::MAX;
    assert_eq!(
        format!("{w:b} {w:o} {w:x} {w:X}"),
        "111111111111 7777 fff FFF"
    );
}

#[test]
fn window_full_names_the_window_size() {
    assert_eq!(
        Error::WindowFull { window_size: ws(4) }.to_string(),
        "Transfer window full (size 4)"
    );
}

#[test]
fn window_reports_its_size_as_a_window_size() {
    assert_eq!(TransferWindow::new(ws(7)).window_size(), ws(7));
    assert_eq!(TransferNumberAllocator::new(ws(7), 0).window_size(), ws(7));
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
    // greatest = 9, window = 4: valid numbers are 6..=9.
    assert_eq!(w.process(0), TransferValidity::OutsideWindow);
    assert_eq!(w.process(6), TransferValidity::InProgress);
    assert_eq!(w.process(5), TransferValidity::OutsideWindow);
}

#[test]
fn new_transfer_boundary_is_half_space_plus_half_window() {
    // Figure 2: T is new iff (T - GREATEST) mod 2^32 < 2^31 + WINDOW_SIZE/2.
    let mut w = TransferWindow::new(ws(16));
    assert_eq!(w.process(0), TransferValidity::New);
    let boundary = (1u32 << 31) + 8;
    assert_eq!(w.process(boundary), TransferValidity::OutsideWindow);
    assert_eq!(w.greatest(), Some(0));
    assert_eq!(w.process(boundary - 1), TransferValidity::New);
    assert_eq!(w.greatest(), Some(boundary - 1));
}

#[test]
fn odd_window_size_rounds_the_margin_down() {
    // WINDOW_SIZE / 2 is integer division: for 5 the margin is 2.
    let mut w = TransferWindow::new(ws(5));
    w.process(0);
    assert_eq!(w.process((1u32 << 31) + 2), TransferValidity::OutsideWindow);
    assert_eq!(w.process((1u32 << 31) + 1), TransferValidity::New);
}

#[test]
fn wraparound() {
    let mut w = TransferWindow::new(ws(16));
    let start = u32::MAX - 5;
    for i in 0..20u32 {
        let t = start.wrapping_add(i);
        assert_eq!(w.process(t), TransferValidity::New, "transfer {t}");
    }
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

#[test]
fn reset_forgets_the_greatest() {
    let mut w = TransferWindow::new(ws(4));
    w.process(1000);
    assert_eq!(w.process(3), TransferValidity::OutsideWindow);
    w.reset();
    assert_eq!(w.greatest(), None);
    assert_eq!(w.process(3), TransferValidity::New);
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
fn window_full() {
    let mut a = TransferNumberAllocator::new(ws(4), 0);
    for _ in 0..4 {
        a.allocate().unwrap();
    }
    assert!(!a.can_allocate());
    assert_eq!(a.allocate(), Err(Error::WindowFull { window_size: ws(4) }));
}

#[test]
fn release_of_oldest_frees_slot() {
    let mut a = TransferNumberAllocator::new(ws(4), 0);
    for _ in 0..4 {
        a.allocate().unwrap();
    }
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

#[cfg(feature = "rand")]
#[test]
fn allocator_from_rng_seeds_first_number() {
    let mut a = TransferNumberAllocator::from_rng(ws(16), &mut common::FixedRng(12345));
    assert_eq!(a.allocate(), Ok(12345));
}

#[cfg(feature = "rand")]
#[test]
fn allocator_try_from_rng_seeds_first_number() {
    let mut a =
        TransferNumberAllocator::try_from_rng(ws(16), &mut common::FixedRng(12345)).unwrap();
    assert_eq!(a.allocate(), Ok(12345));
}

#[cfg(feature = "rand")]
#[test]
fn allocator_try_from_rng_returns_the_rng_error() {
    assert_eq!(
        TransferNumberAllocator::try_from_rng(ws(16), &mut common::FailingRng).err(),
        Some(common::RngFailure)
    );
}

#[test]
fn allocator_wraps() {
    let mut a = TransferNumberAllocator::new(ws(4), u32::MAX - 1);
    assert_eq!(a.allocate(), Ok(u32::MAX - 1));
    assert_eq!(a.allocate(), Ok(u32::MAX));
    assert_eq!(a.allocate(), Ok(0));
    assert_eq!(a.allocate(), Ok(1));
}
