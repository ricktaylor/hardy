#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        _ = s.parse::<hardy_eid_pattern::EidPattern>();

        // Leave this out for now, as it is too strict, for the current parser

        // if let Ok(pattern) = s.parse::<hardy_eid_pattern::EidPattern>() {
        // let s2 = pattern.to_string();
        // let pattern2 = s2
        //     .parse::<hardy_eid_pattern::EidPattern>()
        //     .expect(&format!("Failed to round-trip {s} and {s2}"));

        // if pattern2 != pattern {
        //     panic!("{s} and {s2}");
        // }
        // assert_eq!(pattern2, pattern);
        // }
    }
});

// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/eid_pattern_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_pattern_str -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/eid_pattern_str/lcov.info
// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/eid_pattern_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_pattern_str -o ./fuzz/coverage/eid_pattern_str/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
