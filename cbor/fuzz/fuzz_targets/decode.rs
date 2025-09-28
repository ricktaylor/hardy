#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    _ = hardy_cbor::decode::parse_value(data, |value, _, _| {
        _ = format!("{value:?}");
        Ok::<_, hardy_cbor::decode::Error>(())
    });
});

// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/decode/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/decode -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/decode/lcov.info
// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/decode/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/decode -o ./fuzz/coverage/decode/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
