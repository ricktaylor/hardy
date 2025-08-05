#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary)]
enum Action {
    Add(String, u8),
    Lookup(String),
    RemoveAll(String),
    RemoveIf(String, u8),
}

fn perform_action(map: &mut hardy_eid_pattern::EidPatternMap<u8>, action: Action) {
    match action {
        Action::Add(pattern, value) => {
            if let Ok(pattern) = pattern.parse::<hardy_eid_pattern::EidPattern>() {
                map.insert(pattern, value);
            }
        }
        Action::Lookup(eid) => {
            if let Ok(eid) = eid.parse::<hardy_bpv7::eid::Eid>() {
                for i in map.find(&eid) {
                    println!("{i}");
                }
            }
        }
        Action::RemoveAll(pattern) => {
            if let Ok(pattern) = pattern.parse::<hardy_eid_pattern::EidPattern>() {
                map.remove::<std::collections::BinaryHeap<_>>(&pattern);
            }
        }
        Action::RemoveIf(pattern, val) => {
            if let Ok(pattern) = pattern.parse::<hardy_eid_pattern::EidPattern>() {
                map.remove_if::<std::collections::BinaryHeap<_>>(&pattern, |b| b == &val);
            }
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if let Ok(actions) = Vec::<Action>::arbitrary(&mut arbitrary::Unstructured::new(data)) {
        let mut map = hardy_eid_pattern::EidPatternMap::new();
        for a in actions {
            perform_action(&mut map, a);
        }
    }
});

// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/eid_pattern_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_pattern_str -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/eid_pattern_str/lcov.info
// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/eid_pattern_str/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/eid_pattern_str -o ./fuzz/coverage/eid_pattern_str/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
