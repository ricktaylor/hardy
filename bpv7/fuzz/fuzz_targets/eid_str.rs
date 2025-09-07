#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data)
        && let Ok(eid) = s.parse::<hardy_bpv7::eid::Eid>()
    {
        let s2 = eid.to_string();
        let eid2 = s2
            .parse::<hardy_bpv7::eid::Eid>()
            .expect("Failed to round-trip");

        if eid2 != eid {
            panic!("{s} and {s2}");
        }
        assert_eq!(eid2, eid);
    }
});

// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/eid_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_str -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/eid_str/lcov.info
// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/eid_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_str -o ./fuzz/coverage/eid_str/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
