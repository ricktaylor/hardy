#![no_main]

use libfuzzer_sys::{Corpus, fuzz_target};

fuzz_target!(|data: &[u8]| -> Corpus {
    if common::service::service_send(data) {
        Corpus::Keep
    } else {
        Corpus::Reject
    }
});

// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/service/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/service -o ./fuzz/coverage/service/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/service/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/service -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/service/lcov.info
