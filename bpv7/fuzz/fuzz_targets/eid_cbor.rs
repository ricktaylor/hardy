#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(eid) = hardy_cbor::decode::parse::<hardy_bpv7::prelude::Eid>(data) {
        format!("{eid:?}");
    }
});
