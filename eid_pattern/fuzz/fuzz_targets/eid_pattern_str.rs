#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        _ = s.parse::<hardy_eid_pattern::EidPattern>();
    }
});

// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/eid_pattern_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_pattern_str -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/eid_pattern_str/lcov.info
// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/eid_pattern_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_pattern_str -o ./fuzz/coverage/eid_pattern_str/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
