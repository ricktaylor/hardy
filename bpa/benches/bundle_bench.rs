use criterion::*;

/* TODO:  This whole benchmarking suite is pretty pointless and not statistically significant
We need to rewrite this into something useful, and not pull in hardy-bpa-fuzz */

fn get_bundle() -> Box<[u8]> {
    let builder: hardy_bpv7::builder::Builder = hardy_bpv7::builder::BundleTemplate {
        source: "ipn:1.1".parse().unwrap(),
        destination: "ipn:2.1".parse().unwrap(),
        report_to: Some("ipn:1.0".parse().unwrap()),
        ..Default::default()
    }
    .into();

    builder
        .with_payload([42; 4096])
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .1
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("bundle-throughput");

    group.throughput(Throughput::Bytes(get_bundle().len() as u64));
    group.bench_function("bundle", |b| {
        b.iter(|| hardy_bpa_fuzz::send_bundle(&*get_bundle()))
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
