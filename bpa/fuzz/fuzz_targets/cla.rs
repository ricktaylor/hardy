#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    common::cla::cla_send(hardy_bpa::Bytes::copy_from_slice(data));
});

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/cla/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/cla -o ./fuzz/coverage/cla/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/cla/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/cla -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/cla/lcov.info
