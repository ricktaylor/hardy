#![no_main]

use libfuzzer_sys::{Corpus, fuzz_target};

fuzz_target!(|data: &[u8]| -> Corpus {
    if common::send(data) {
        Corpus::Keep
    } else {
        Corpus::Reject
    }
});

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/bpa/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/bpa -o ./fuzz/coverage/bpa/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/bpa/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/bpa -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/bpa/lcov.info
