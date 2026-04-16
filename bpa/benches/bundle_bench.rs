//! BPA pipeline throughput benchmark.
//!
//! Uses criterion to measure bundle forwarding throughput through the
//! full BPA pipeline with in-memory storage.

use criterion::*;
use hardy_bpa::bpa::{Bpa, BpaRegistration};
use hardy_bpa::cla;
use hardy_bpa::{Bytes, async_trait};
use hardy_bpv7::eid::{IpnNodeId, NodeId};
use std::sync::Arc;

// -- Inline CLA — captures arrival time inside forward() --

struct BenchCla {
    sink: hardy_async::sync::spin::Once<Box<dyn cla::Sink>>,
    arrival_tx: flume::Sender<std::time::Instant>,
}

impl BenchCla {
    fn new() -> (Arc<Self>, flume::Receiver<std::time::Instant>) {
        let (tx, rx) = flume::bounded(4096);
        (
            Arc::new(Self {
                sink: hardy_async::sync::spin::Once::new(),
                arrival_tx: tx,
            }),
            rx,
        )
    }
}

#[async_trait]
impl cla::Cla for BenchCla {
    async fn on_register(&self, sink: Box<dyn cla::Sink>, _node_ids: &[NodeId]) {
        self.sink.call_once(|| sink);
    }
    async fn on_unregister(&self) {}
    async fn forward(
        &self,
        _queue: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle: Bytes,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let _ = self.arrival_tx.send(std::time::Instant::now());
        Ok(cla::ForwardBundleResult::Sent)
    }
}

// -- Static state: runtime + BPA live for the entire benchmark process --

struct BenchState {
    cla: Arc<BenchCla>,
    arrival_rx: flume::Receiver<std::time::Instant>,
}

fn get_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
    })
}

fn get_state() -> &'static BenchState {
    static STATE: std::sync::OnceLock<BenchState> = std::sync::OnceLock::new();
    STATE.get_or_init(|| {
        let rt = get_runtime();

        rt.block_on(async {
            let node_ids = hardy_bpa::node_ids::NodeIds::try_from(
                [NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 1,
                })]
                .as_slice(),
            )
            .unwrap();

            let bpa = Bpa::builder().node_ids(node_ids).build().await.unwrap();
            bpa.start(false);

            let (cla, arrival_rx) = BenchCla::new();
            bpa.register_cla("bench".to_string(), cla.clone(), None)
                .await
                .unwrap();

            let remote_node = NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 2,
            });
            cla.sink
                .get()
                .unwrap()
                .add_peer(
                    cla::ClaAddress::Private("peer".as_bytes().into()),
                    &[remote_node],
                )
                .await
                .unwrap();

            // BPA intentionally not shut down — lives for process lifetime (like fuzz harness)
            std::mem::forget(bpa);

            BenchState { cla, arrival_rx }
        })
    })
}

fn print_system_info() {
    use std::fs;
    if let Ok(cpuinfo) = fs::read_to_string("/proc/cpuinfo") {
        if let Some(model) = cpuinfo
            .lines()
            .find(|l| l.starts_with("model name"))
            .and_then(|l| l.split(':').nth(1))
        {
            eprintln!("CPU: {}", model.trim());
        }
    }
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(0);
    eprintln!("Cores: {cores}");
    if let Ok(meminfo) = fs::read_to_string("/proc/meminfo") {
        if let Some(total) = meminfo
            .lines()
            .find(|l| l.starts_with("MemTotal"))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
        {
            eprintln!("RAM: {} GB", total / 1_048_576);
        }
    }
    eprintln!("Arch: {}", std::env::consts::ARCH);
    eprintln!(
        "Profile: {}",
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    eprintln!("Date: {:?}", std::time::SystemTime::now());
}

fn throughput_benchmark(c: &mut Criterion) {
    print_system_info();
    let rt = get_runtime();
    eprintln!("Tokio workers: {}", rt.metrics().num_workers());
    let state = get_state();

    let mut group = c.benchmark_group("bpa-pipeline");
    group.throughput(Throughput::Elements(1));

    let src: hardy_bpv7::eid::Eid = "ipn:0.3.1".parse().unwrap();
    let dst: hardy_bpv7::eid::Eid = "ipn:0.2.99".parse().unwrap();
    let payload = [42u8; 1024];

    group.bench_function("forward-1bundle", |b| {
        b.iter(|| {
            let (_, data) = hardy_bpv7::builder::Builder::new(src.clone(), dst.clone())
                .with_payload(std::borrow::Cow::Borrowed(&payload))
                .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
                .unwrap();

            rt.block_on(async {
                state
                    .cla
                    .sink
                    .get()
                    .unwrap()
                    .dispatch(Bytes::from(data), None, None)
                    .await
                    .unwrap();
                state.arrival_rx.recv_async().await.unwrap();
            });
        })
    });

    group.finish();
}

criterion_group!(benches, throughput_benchmark);
criterion_main!(benches);
