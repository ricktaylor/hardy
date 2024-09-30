#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    match hardy_cbor::decode::parse::<hardy_bpv7::prelude::ValidBundle>(data) {
        Ok(_) => assert!(hardy_bpv7::prelude::Bundle::could_be_bundle(data).is_ok()),
        Err(_) => {}
    }
});

// llvm-cov show --format=html  -instr-profile ./fuzz/coverage/bundle/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/bundle -o ./fuzz/coverage/bundle/ -ignore-filename-regex='/.cargo/|rustc/'
