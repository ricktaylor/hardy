use criterion::*;

fn get_bundle() -> Box<[u8]> {
    let builder: hardy_bpv7::builder::Builder = hardy_bpv7::builder::BundleTemplate {
        source: "ipn:1.1".parse().unwrap(),
        destination: "ipn:2.1".parse().unwrap(),
        report_to: Some("ipn:1.0".parse().unwrap()),
        flags: None,
        crc_type: None,
        lifetime: None,
        hop_limit: None,
    }
    .into();

    builder
        .add_extension_block(hardy_bpv7::block::Type::Payload)
        .with_flags(hardy_bpv7::block::Flags {
            delete_bundle_on_failure: true,
            ..Default::default()
        })
        .build([42; 4096])
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .1
}

fn bench(c: &mut Criterion) {
    let mut group = c.benchmark_group("bundle-throughput");

    group.throughput(Throughput::Bytes(get_bundle().len() as u64));
    group.bench_with_input("bundle", &*get_bundle(), |b, bundle| {
        b.iter(|| hardy_bpa_fuzz::send_bundle(bundle))
    });
    group.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
