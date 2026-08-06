//! Criterion benchmarks for the CBOR encoder.
//!
//! Measures emit_uint_minor (the highest-frequency encoder function) and
//! full bundle-like encoding workloads.

use criterion::*;
use hardy_cbor::encode::{self, Encoder};

/// Benchmark encoding a single u64 value across different magnitude ranges.
fn bench_emit_uint(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode/uint");

    // Tiny value (inline, 1 byte total)
    group.bench_function("u8_inline", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            e.emit(black_box(&23u64));
            black_box(e.build());
        })
    });

    // 1-byte value (2 bytes total: marker + 1)
    group.bench_function("u8_extended", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            e.emit(black_box(&200u64));
            black_box(e.build());
        })
    });

    // 2-byte value (3 bytes total: marker + 2)
    group.bench_function("u16", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            e.emit(black_box(&1000u64));
            black_box(e.build());
        })
    });

    // 4-byte value (5 bytes total: marker + 4)
    group.bench_function("u32", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            e.emit(black_box(&100_000u64));
            black_box(e.build());
        })
    });

    // 8-byte value (9 bytes total: marker + 8)
    group.bench_function("u64", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            e.emit(black_box(&(u64::MAX - 1)));
            black_box(e.build());
        })
    });

    group.finish();
}

/// Benchmark encoding a sequence of mixed integers (simulates encoding
/// a bundle's primary block fields).
fn bench_emit_mixed_integers(c: &mut Criterion) {
    c.bench_function("encode/mixed_integers_10", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            // Simulate primary block: version, flags, crc, dest, source, etc.
            e.emit(black_box(&7u64)); // version
            e.emit(black_box(&0u64)); // flags
            e.emit(black_box(&1u64)); // crc type
            e.emit(black_box(&2u64)); // dest scheme
            e.emit(black_box(&1u64)); // dest allocator
            e.emit(black_box(&42u64)); // dest node
            e.emit(black_box(&7u64)); // dest service
            e.emit(black_box(&1000u64)); // creation time (ms)
            e.emit(black_box(&3600000u64)); // lifetime (ms)
            e.emit(black_box(&0u64)); // sequence
            black_box(e.build());
        })
    });
}

/// Benchmark encoding a CBOR array with nested integers (bundle structure).
fn bench_emit_array(c: &mut Criterion) {
    c.bench_function("encode/array_10_elements", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            e.emit_array(Some(10), |a| {
                a.emit(black_box(&7u64));
                a.emit(black_box(&0u64));
                a.emit(black_box(&1u64));
                a.emit(black_box(&2u64));
                a.emit(black_box(&1u64));
                a.emit(black_box(&42u64));
                a.emit(black_box(&7u64));
                a.emit(black_box(&1000u64));
                a.emit(black_box(&3600000u64));
                a.emit(black_box(&0u64));
            });
            black_box(e.build());
        })
    });
}

/// Benchmark encoding a byte string (simulates payload block).
fn bench_emit_bytes(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode/bytes");

    let small_payload = vec![0xABu8; 64];
    let medium_payload = vec![0xABu8; 1024];
    let large_payload = vec![0xABu8; 65536];

    group.bench_function("64B", |b| {
        b.iter(|| {
            let (bytes, _) = encode::emit(&encode::Bytes(black_box(&small_payload)));
            black_box(bytes);
        })
    });

    group.bench_function("1KB", |b| {
        b.iter(|| {
            let (bytes, _) = encode::emit(&encode::Bytes(black_box(&medium_payload)));
            black_box(bytes);
        })
    });

    group.bench_function("64KB", |b| {
        b.iter(|| {
            let (bytes, _) = encode::emit(&encode::Bytes(black_box(&large_payload)));
            black_box(bytes);
        })
    });

    group.finish();
}

/// Benchmark encoding a realistic bundle-like structure:
/// array(primary_block, payload_block)
fn bench_emit_bundle_like(c: &mut Criterion) {
    let payload = vec![0x42u8; 1024];

    c.bench_function("encode/bundle_like_1KB", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            e.emit_array(Some(2), |a| {
                // Primary block (array of integers)
                a.emit_array(Some(8), |primary| {
                    primary.emit(&7u64); // version
                    primary.emit(&0u64); // flags
                    primary.emit(&1u64); // crc type
                    primary.emit(&2u64); // dest scheme
                    primary.emit(&42u64); // dest node
                    primary.emit(&7u64); // dest service
                    primary.emit(&1000u64); // creation time
                    primary.emit(&3600000u64); // lifetime
                });
                // Payload block (byte string)
                a.emit(&encode::Bytes(black_box(&payload)));
            });
            black_box(e.build());
        })
    });
}

/// Benchmark with_capacity vs new for known-size encoding.
fn bench_with_capacity(c: &mut Criterion) {
    let mut group = c.benchmark_group("encode/capacity");

    group.bench_function("new_then_encode_50B", |b| {
        b.iter(|| {
            let mut e = Encoder::new();
            for i in 0..10u64 {
                e.emit(black_box(&i));
            }
            black_box(e.build());
        })
    });

    group.bench_function("with_capacity_then_encode_50B", |b| {
        b.iter(|| {
            let mut e = Encoder::with_capacity(50);
            for i in 0..10u64 {
                e.emit(black_box(&i));
            }
            black_box(e.build());
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_emit_uint,
    bench_emit_mixed_integers,
    bench_emit_array,
    bench_emit_bytes,
    bench_emit_bundle_like,
    bench_with_capacity,
);
criterion_main!(benches);
