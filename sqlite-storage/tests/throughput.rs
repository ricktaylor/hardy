//! Throughput and write-latency benchmark for the SQLite metadata store.
//!
//! Not part of the test suite: it is `#[ignore]`d, takes tens of seconds,
//! and measures the machine it runs on. Run it in release, with the
//! database on the disk under test (not a tmpfs):
//!
//! ```sh
//! BENCH_DIR=/var/tmp cargo test --release -p hardy-sqlite-storage \
//!     --test throughput -- --ignored --nocapture
//! ```
//!
//! Each simulated bundle makes the metadata calls of one forwarding
//! lifecycle, in order: insert, two status swaps, a get, a conditional
//! tombstone and a tombstone. That is five writes and one read. Two modes:
//!
//! - **Closed loop** (`BENCH_RATE` unset): `BENCH_TASKS` tasks each run
//!   their bundles back to back, which measures peak throughput.
//! - **Fixed rate** (`BENCH_RATE` bundles per second, across all tasks):
//!   each task starts a bundle on a schedule, so latency tails show the
//!   store's stalls, not queueing behind a saturated store.
//!
//! Alongside, a task on the runtime wakes every millisecond and records
//! how late it woke: the time the runtime's worker threads were not
//! available, which is what a blocking store call costs everything else on
//! the runtime.
//!
//! After each run it times one expiry poll that stops at the first live
//! bundle, as the reaper's does. With `BENCH_TOMBSTONES` set, that many
//! tombstones are written first, all sorting ahead of every live bundle, so
//! the poll measures what tombstones cost it.
//!
//! Environment: `BENCH_TASKS` (64), `BENCH_BUNDLES` (25,600),
//! `BENCH_PREFILL` live bundles inserted first (10,000), `BENCH_RATE`
//! (closed loop), `BENCH_TOMBSTONES` (0), `BENCH_WORKERS` runtime worker
//! threads (4), `BENCH_RUNS` (2), `BENCH_DIR` (the system temporary
//! directory).

use std::{
    env::{temp_dir, var},
    fs::metadata,
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::available_parallelism,
    time::{Duration, Instant},
};

use hardy_bpa::{
    async_trait,
    bundle::{Bundle, BundleMetadata, BundleStatus},
    storage::MetadataStorage,
    stream::{SendError, Sender},
};
use hardy_bpv7::{builder::Builder, creation_timestamp::CreationTimestamp, eid::Eid};
use hardy_sqlite_storage::SqliteStorage;
use rusqlite::version;
use tempfile::tempdir_in;
use time::{Duration as TimeDuration, OffsetDateTime};
use tokio::{
    runtime::Builder as RuntimeBuilder,
    task::{JoinHandle, spawn},
    time::{sleep, sleep_until},
};

struct Config {
    tasks: u64,
    bundles: u64,
    prefill: u64,
    // Bundles per second across all tasks; `None` runs a closed loop.
    rate: Option<f64>,
    tombstones: u64,
    dir: PathBuf,
}

impl Config {
    fn from_env() -> Self {
        Self {
            tasks: env("BENCH_TASKS", 64),
            bundles: env("BENCH_BUNDLES", 25_600),
            prefill: env("BENCH_PREFILL", 10_000),
            rate: var("BENCH_RATE").ok().and_then(|rate| rate.parse().ok()),
            tombstones: env("BENCH_TOMBSTONES", 0),
            dir: var("BENCH_DIR").map_or_else(|_| temp_dir(), PathBuf::from),
        }
    }
}

fn env<T: FromStr>(key: &str, default: T) -> T {
    var(key)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[test]
#[ignore = "a benchmark: run explicitly, in release"]
fn throughput() {
    let runtime = RuntimeBuilder::new_multi_thread()
        .worker_threads(env("BENCH_WORKERS", 4))
        .enable_all()
        .build()
        .unwrap();
    let config = Config::from_env();
    println!(
        "SQLite {}, {} logical CPUs",
        version(),
        available_parallelism().map_or(0, |n| n.get())
    );
    for _ in 0..env("BENCH_RUNS", 2) {
        runtime.block_on(run(&config));
    }
}

async fn run(config: &Config) {
    let dir = tempdir_in(&config.dir).unwrap();
    let store: Arc<dyn MetadataStorage> =
        Arc::new(SqliteStorage::new(Some(dir.path().into()), None, true));
    tombstones(&store, config.tombstones).await;
    prefill(&store, config.prefill).await;

    let stop = Arc::new(AtomicBool::new(false));
    let ticker = spawn(tick_lateness(stop.clone()));
    let wal = dir.path().join("metadata.db-wal");
    let peak_wal = Arc::new(AtomicU64::new(0));
    let sampler = spawn(sample_peak(wal.clone(), peak_wal.clone(), stop.clone()));

    let started = Instant::now();
    let latencies = drive(&store, config).await;
    let elapsed = started.elapsed().as_secs_f64();
    stop.store(true, Ordering::Relaxed);
    let lateness = ticker.await.unwrap();
    sampler.await.unwrap();

    let bundles = (config.bundles / config.tasks * config.tasks) as f64;
    println!(
        "tasks={} rate={} | {:.0} bundles/s, {:.0} writes/s | WAL peak {:.0} MiB, end {:.0} MiB",
        config.tasks,
        config
            .rate
            .map_or_else(|| "closed".into(), |rate| format!("{rate}/s")),
        bundles / elapsed,
        bundles * 5.0 / elapsed,
        peak_wal.load(Ordering::Relaxed) as f64 / 1_048_576.0,
        mebibytes(&wal),
    );
    println!("   writes: {}", tail(latencies));
    println!("   runtime tick lateness: {}", tail(lateness));

    let timed = Instant::now();
    store.poll_expiry(&FirstOnly).await.unwrap();
    println!(
        "   expiry poll to the first live bundle: {:.1} ms",
        timed.elapsed().as_secs_f64() * 1e3
    );
}

// Closes the stream at the first bundle, as the reaper does once it has the
// next expiry.
struct FirstOnly;

#[async_trait]
impl Sender<Bundle> for FirstOnly {
    async fn send(&self, bundle: Bundle) -> Result<(), SendError<Bundle>> {
        Err(SendError(bundle))
    }
}

// Tombstones that sort ahead of every live bundle by expiry: bundles
// created a day before the run, each with its own sequence number, inserted
// and then tombstoned.
async fn tombstones(store: &Arc<dyn MetadataStorage>, count: u64) {
    let (earlier, _) =
        CreationTimestamp::try_from(OffsetDateTime::now_utc() - TimeDuration::days(1))
            .unwrap()
            .into_parts();
    let fill: Vec<JoinHandle<()>> = (0..64)
        .map(|task| {
            let store = store.clone();
            spawn(async move {
                for n in (task..count).step_by(64) {
                    let created = CreationTimestamp::from_parts(earlier, n);
                    let bundle = make_bundle_at(n, BundleStatus::Waiting, created);
                    assert!(store.insert(&bundle).await.unwrap());
                    store.tombstone(bundle.id()).await.unwrap();
                }
            })
        })
        .collect();
    for task in fill {
        task.await.unwrap();
    }
}

// Live bundles for the run's writes to share the indexes with.
async fn prefill(store: &Arc<dyn MetadataStorage>, count: u64) {
    let fill: Vec<JoinHandle<()>> = (0..64)
        .map(|task| {
            let store = store.clone();
            spawn(async move {
                for n in (task..count).step_by(64) {
                    let bundle = make_bundle(1_000_000 + n, BundleStatus::Waiting);
                    store.insert(&bundle).await.unwrap();
                }
            })
        })
        .collect();
    for task in fill {
        task.await.unwrap();
    }
}

// Runs every task's bundles, and returns every write's latency in µs.
async fn drive(store: &Arc<dyn MetadataStorage>, config: &Config) -> Vec<u32> {
    let per_task = config.bundles / config.tasks;
    let gap = config
        .rate
        .map(|rate| Duration::from_secs_f64(config.tasks as f64 / rate));
    let tasks: Vec<JoinHandle<Vec<u32>>> = (0..config.tasks)
        .map(|task| {
            let store = store.clone();
            let bundles: Vec<Bundle> = (0..per_task)
                .map(|i| make_bundle(10 + task * per_task + i, BundleStatus::Dispatching))
                .collect();
            // Stagger the tasks' schedules across one gap.
            let offset = gap.map_or(Duration::ZERO, |gap| {
                gap.mul_f64(task as f64 / config.tasks as f64)
            });
            spawn(async move {
                let mut latencies = Vec::with_capacity(bundles.len() * 5);
                let mut due = Instant::now() + offset;
                for (n, bundle) in bundles.into_iter().enumerate() {
                    if let Some(gap) = gap {
                        sleep_until(due.into()).await;
                        due += gap;
                    }
                    let peer = (n % 8) as u32;
                    lifecycle(&*store, bundle, peer, &mut latencies).await;
                }
                latencies
            })
        })
        .collect();
    let mut latencies = Vec::new();
    for task in tasks {
        latencies.extend(task.await.unwrap());
    }
    latencies
}

// One bundle's forwarding lifecycle, recording each write's latency.
async fn lifecycle(
    store: &dyn MetadataStorage,
    mut bundle: Bundle,
    peer: u32,
    latencies: &mut Vec<u32>,
) {
    let timed = Instant::now();
    assert!(store.insert(&bundle).await.unwrap());
    latencies.push(micros(timed));

    for status in [
        BundleStatus::ForwardPending {
            peer,
            queue: 0,
            next_hop: "ipn:3.0".parse().unwrap(),
        },
        BundleStatus::ForwardAckPending { peer },
    ] {
        let timed = Instant::now();
        assert!(
            store
                .swap_status(bundle.id(), &bundle.status, &status)
                .await
                .unwrap()
        );
        latencies.push(micros(timed));
        bundle.status = status;
    }

    assert!(store.get(bundle.id()).await.unwrap().is_some());

    let timed = Instant::now();
    assert!(
        store
            .tombstone_if(bundle.id(), &bundle.status)
            .await
            .unwrap()
    );
    latencies.push(micros(timed));
    let timed = Instant::now();
    store.tombstone(bundle.id()).await.unwrap();
    latencies.push(micros(timed));
}

fn make_bundle(n: u64, status: BundleStatus) -> Bundle {
    make_bundle_at(n, status, CreationTimestamp::now())
}

fn make_bundle_at(n: u64, status: BundleStatus, created: CreationTimestamp) -> Bundle {
    let source: Eid = "ipn:1.0".parse().unwrap();
    let destination: Eid = format!("ipn:2.{n}").parse().unwrap();
    let (bpv7, _) = Builder::new(source, destination)
        .with_payload(b"benchmark payload".to_vec().into())
        .build(created)
        .unwrap();
    Bundle {
        bpv7,
        metadata: BundleMetadata::originated(),
        status,
    }
}

// How late, in µs, a task that asks to wake every millisecond wakes, until
// `stop` is set.
async fn tick_lateness(stop: Arc<AtomicBool>) -> Vec<u32> {
    let mut lateness = Vec::new();
    while !stop.load(Ordering::Relaxed) {
        let due = Instant::now() + Duration::from_millis(1);
        sleep_until(due.into()).await;
        lateness.push(micros(due));
    }
    lateness
}

// The largest size, in bytes, the file at `path` reaches until `stop` is
// set.
async fn sample_peak(path: PathBuf, peak: Arc<AtomicU64>, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Relaxed) {
        peak.fetch_max(metadata(&path).map_or(0, |m| m.len()), Ordering::Relaxed);
        sleep(Duration::from_millis(20)).await;
    }
}

fn micros(since: Instant) -> u32 {
    u32::try_from(since.elapsed().as_micros()).unwrap_or(u32::MAX)
}

fn mebibytes(path: &Path) -> f64 {
    metadata(path).map_or(0.0, |m| m.len() as f64 / 1_048_576.0)
}

// Percentiles and counts over a set of latencies in µs.
fn tail(mut latencies: Vec<u32>) -> String {
    if latencies.is_empty() {
        return "-".into();
    }
    latencies.sort_unstable();
    let at = |p: f64| latencies[((latencies.len() - 1) as f64 * p) as usize];
    format!(
        "p50 {} µs, p99 {} µs, p99.9 {} µs, max {:.1} ms; {} over 5 ms, {} over 20 ms",
        at(0.5),
        at(0.99),
        at(0.999),
        f64::from(*latencies.last().unwrap()) / 1e3,
        latencies.iter().filter(|&&l| l > 5_000).count(),
        latencies.iter().filter(|&&l| l > 20_000).count(),
    )
}
