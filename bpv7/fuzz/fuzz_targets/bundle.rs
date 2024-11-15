#![no_main]

use hardy_bpv7::prelude::*;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut f = |_: &Eid, _| Ok(None);

    if let Ok(ValidBundle::Rewritten(_, data, _)) = ValidBundle::parse(data, &mut f) {
        let Ok(ValidBundle::Valid(..)) = ValidBundle::parse(&data, &mut f) else {
            panic!("Rewrite borked");
        };
    }
});

// llvm-cov show --format=html  -instr-profile ./fuzz/coverage/bundle/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/bundle -o ./fuzz/coverage/bundle/ -ignore-filename-regex='/.cargo/|rustc/'
