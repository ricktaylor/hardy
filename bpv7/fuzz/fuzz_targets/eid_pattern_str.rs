#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data) {
        _ = s.parse::<hardy_bpv7::prelude::EidPattern>();
    }
});
