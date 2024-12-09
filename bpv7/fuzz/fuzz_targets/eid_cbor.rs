#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(eid) = hardy_cbor::decode::parse::<hardy_bpv7::prelude::Eid>(data) {
        _ = format!("{eid:?}");
    }
});

// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/eid_cbor/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_cbor -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/eid_cbor/lcov.info
// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/eid_cbor/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_cbor -o ./fuzz/coverage/eid_cbor/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
