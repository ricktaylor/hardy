//! Criterion benchmarks for the send path.
//!
//! Measures packing throughput for small bundles and for one segmented
//! bundle, into a new buffer per PDU, into a slice the caller keeps, and as
//! slices for the caller to gather, the cost of draining messages queued
//! behind transfers that are waiting on their producers, and of full PDUs
//! with ready transfers queued behind, packed each way.

use std::hint::black_box;

use bytes::Bytes;
use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use hardy_btpu::{
    sender::{CarriedList, PduSlices, SendOptions, Sender, SenderConfig},
    transfer::WindowSize,
};

const SMALL_BUNDLE_LEN: usize = 100;
const SMALL_BUNDLES: usize = 1024;
const LARGE_BUNDLE_LEN: usize = 1 << 20;

fn sender(window_size: WindowSize) -> Sender {
    Sender::new(
        SenderConfig {
            window_size,
            ..SenderConfig::default()
        },
        0,
    )
}

fn enqueue_small(s: &mut Sender, count: usize) {
    let bundle = Bytes::from(vec![0xA5; SMALL_BUNDLE_LEN]);
    for _ in 0..count {
        s.enqueue(bundle.clone(), SendOptions::default()).unwrap();
    }
}

fn drain(s: &mut Sender) {
    while let Some(pdu) = s.next_pdu() {
        black_box(pdu);
    }
}

fn drain_write(s: &mut Sender) {
    let mut carried = CarriedList::new();
    let mut out = vec![0; SenderConfig::default().pdu_size.get()];
    while let Some(len) = s.next_pdu_write(&mut carried, &mut out).unwrap() {
        black_box((&out[..len], &carried));
    }
}

fn drain_vectored(s: &mut Sender) {
    let mut carried = CarriedList::new();
    let mut out = PduSlices::new();
    while s.next_pdu_vectored(&mut carried, &mut out).is_some() {
        black_box((&out, &carried));
    }
}

/// Pack fitting bundles, many to a PDU.
fn bench_small_bundles(c: &mut Criterion) {
    let mut group = c.benchmark_group("sender/small_bundles");
    group.throughput(Throughput::Bytes((SMALL_BUNDLE_LEN * SMALL_BUNDLES) as u64));
    group.bench_function("drain", |b| {
        b.iter_batched(
            || {
                let mut s = sender(WindowSize::DEFAULT);
                enqueue_small(&mut s, SMALL_BUNDLES);
                s
            },
            |mut s| drain(&mut s),
            BatchSize::LargeInput,
        )
    });
    group.bench_function("drain_write", |b| {
        b.iter_batched(
            || {
                let mut s = sender(WindowSize::DEFAULT);
                enqueue_small(&mut s, SMALL_BUNDLES);
                s
            },
            |mut s| drain_write(&mut s),
            BatchSize::LargeInput,
        )
    });
    group.bench_function("drain_vectored", |b| {
        b.iter_batched(
            || {
                let mut s = sender(WindowSize::DEFAULT);
                enqueue_small(&mut s, SMALL_BUNDLES);
                s
            },
            |mut s| drain_vectored(&mut s),
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

/// Cut one fully pushed bundle into segments, one to a PDU.
fn bench_segmented_bundle(c: &mut Criterion) {
    let mut group = c.benchmark_group("sender/segmented_bundle");
    group.throughput(Throughput::Bytes(LARGE_BUNDLE_LEN as u64));
    let bundle = Bytes::from(vec![0xA5; LARGE_BUNDLE_LEN]);
    group.bench_function("drain", |b| {
        b.iter_batched(
            || {
                let mut s = sender(WindowSize::DEFAULT);
                s.enqueue(bundle.clone(), SendOptions::default()).unwrap();
                s
            },
            |mut s| drain(&mut s),
            BatchSize::LargeInput,
        )
    });
    group.bench_function("drain_write", |b| {
        b.iter_batched(
            || {
                let mut s = sender(WindowSize::DEFAULT);
                s.enqueue(bundle.clone(), SendOptions::default()).unwrap();
                s
            },
            |mut s| drain_write(&mut s),
            BatchSize::LargeInput,
        )
    });
    group.bench_function("drain_vectored", |b| {
        b.iter_batched(
            || {
                let mut s = sender(WindowSize::DEFAULT);
                s.enqueue(bundle.clone(), SendOptions::default()).unwrap();
                s
            },
            |mut s| drain_vectored(&mut s),
            BatchSize::LargeInput,
        )
    });
    group.finish();
}

/// Drain small bundles queued behind `waiting` transfers, each begun and
/// pushed too few bytes to supply a segment, so every PDU passes them over.
fn bench_behind_waiting_transfers(c: &mut Criterion) {
    let mut group = c.benchmark_group("sender/behind_waiting_transfers");
    group.throughput(Throughput::Bytes((SMALL_BUNDLE_LEN * SMALL_BUNDLES) as u64));
    let chunk = Bytes::from(vec![0xA5; SMALL_BUNDLE_LEN]);
    for waiting in [0usize, 64, 1024, 4000] {
        group.bench_with_input(
            BenchmarkId::from_parameter(waiting),
            &waiting,
            |b, &waiting| {
                b.iter_batched(
                    || {
                        let mut s = sender(WindowSize::MAX);
                        for _ in 0..waiting {
                            // A dropped handle leaves its transfer queued.
                            let mut h = s.begin(LARGE_BUNDLE_LEN, SendOptions::default()).unwrap();
                            s.push(&mut h, chunk.clone()).unwrap();
                        }
                        enqueue_small(&mut s, SMALL_BUNDLES);
                        s
                    },
                    |mut s| drain(&mut s),
                    BatchSize::LargeInput,
                )
            },
        );
    }
    group.finish();
}

/// Drain `ready` fully pushed segmented bundles, queued at once: each PDU
/// is one segment that fills it, with the other transfers queued behind.
fn bench_full_pdus_behind_ready_transfers(c: &mut Criterion) {
    const BUNDLE_LEN: usize = 64 * 1024;
    let mut group = c.benchmark_group("sender/full_pdus_behind_ready_transfers");
    let bundle = Bytes::from(vec![0xA5; BUNDLE_LEN]);
    let queued = |ready| {
        let mut s = sender(WindowSize::DEFAULT);
        for _ in 0..ready {
            s.enqueue(bundle.clone(), SendOptions::default()).unwrap();
        }
        s
    };
    for ready in [1usize, 16] {
        group.throughput(Throughput::Bytes((BUNDLE_LEN * ready) as u64));
        group.bench_with_input(BenchmarkId::new("drain", ready), &ready, |b, &ready| {
            b.iter_batched(
                || queued(ready),
                |mut s| drain(&mut s),
                BatchSize::LargeInput,
            )
        });
        group.bench_with_input(
            BenchmarkId::new("drain_write", ready),
            &ready,
            |b, &ready| {
                b.iter_batched(
                    || queued(ready),
                    |mut s| drain_write(&mut s),
                    BatchSize::LargeInput,
                )
            },
        );
        group.bench_with_input(
            BenchmarkId::new("drain_vectored", ready),
            &ready,
            |b, &ready| {
                b.iter_batched(
                    || queued(ready),
                    |mut s| drain_vectored(&mut s),
                    BatchSize::LargeInput,
                )
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_small_bundles,
    bench_segmented_bundle,
    bench_behind_waiting_transfers,
    bench_full_pdus_behind_ready_transfers
);
criterion_main!(benches);
