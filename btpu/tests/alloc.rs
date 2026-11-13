//! Allocation counts of the sender's drain, measured with a counting
//! global allocator.  A binary of its own, so the allocator counts nothing
//! of the other integration tests.

mod common;

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    task::{Context, Waker},
};

use hardy_btpu::sender::{CarriedList, PduSlices, Sender};

use self::common::{bundle, enqueue, sender};

thread_local! {
    // Allocations made by this thread.  Per thread, so that the test
    // harness and other tests in the binary are not counted.  Const
    // initialised and without a destructor, so reading it never allocates.
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// The system allocator, counting each allocation and each reallocation
// made by the calling thread.
struct Counting;

impl Counting {
    fn count() {
        // The thread-local is gone while a thread is torn down; what is
        // allocated then is no test's.
        let _ = ALLOCATIONS.try_with(|n| n.set(n.get() + 1));
    }
}

// SAFETY: every call is forwarded unchanged to `System`, which upholds the
// `GlobalAlloc` contract; counting touches only a thread-local `Cell`.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

// The allocations this thread makes while `f` runs.
fn allocations<R>(f: impl FnOnce() -> R) -> (u64, R) {
    let before = ALLOCATIONS.get();
    let result = f();
    (ALLOCATIONS.get() - before, result)
}

// A sender with 64-byte PDUs that has packed a segmented bundle and one
// that fits a PDU, so its scratch state is warm and its queue empty.
fn warm_sender() -> Sender {
    let mut sender = sender(64, 4);
    enqueue(&mut sender, bundle(200)).unwrap();
    enqueue(&mut sender, bundle(20)).unwrap();
    let mut carried = CarriedList::new();
    while sender.next_pdu_into(&mut carried).is_some() {}
    assert!(!sender.has_pending());
    sender
}

// The counter is live: a PDU packed into `Bytes` takes its buffer from
// the allocator.  Without this, a counter that never moved would pass the
// test below.
#[test]
fn packing_a_pdu_into_bytes_allocates_its_buffer() {
    let mut sender = warm_sender();
    enqueue(&mut sender, bundle(20)).unwrap();
    let mut carried = CarriedList::new();
    let (count, pdu) = allocations(|| sender.next_pdu_into(&mut carried));
    assert_eq!(pdu.map(|pdu| pdu.len()), Some(64));
    assert_eq!(count, 1);
}

// The calls of `f`, out of `CALLS`, that returned true, having asserted
// that none of them allocated.
fn hits_without_allocating(mut f: impl FnMut() -> bool) -> usize {
    let (count, hits) = allocations(|| (0..CALLS).filter(|_| f()).count());
    assert_eq!(count, 0);
    hits
}

const CALLS: usize = 100;

#[test]
fn an_empty_drain_allocates_nothing() {
    let mut sender = warm_sender();
    let mut carried = CarriedList::new();
    let packed = hits_without_allocating(|| sender.next_pdu_into(&mut carried).is_some());
    assert_eq!(packed, 0);

    // `next_pdu` makes its own list, which starts inline.
    assert_eq!(hits_without_allocating(|| sender.next_pdu().is_some()), 0);

    let mut slot = vec![0; 64];
    let packed = hits_without_allocating(|| {
        sender
            .next_pdu_write(&mut carried, &mut slot)
            .unwrap()
            .is_some()
    });
    assert_eq!(packed, 0);

    let mut slices = PduSlices::default();
    let packed = hits_without_allocating(|| {
        sender
            .next_pdu_vectored(&mut carried, &mut slices)
            .is_some()
    });
    assert_eq!(packed, 0);
}

// What a drain that reserves room first calls on every wake.  It parks on
// each call here, registering the waker again.
#[test]
fn polling_for_a_ready_pdu_allocates_nothing() {
    let mut sender = warm_sender();
    let mut cx = Context::from_waker(Waker::noop());
    let ready = hits_without_allocating(|| sender.poll_pdu_ready(&mut cx).is_ready());
    assert_eq!(ready, 0);

    // Nor when a PDU is ready, with a segmented bundle and one that fits
    // queued.
    enqueue(&mut sender, bundle(200)).unwrap();
    enqueue(&mut sender, bundle(20)).unwrap();
    let ready = hits_without_allocating(|| sender.poll_pdu_ready(&mut cx).is_ready());
    assert_eq!(ready, CALLS);
}
