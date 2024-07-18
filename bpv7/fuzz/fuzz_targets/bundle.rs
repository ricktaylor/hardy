#![no_main]

use hardy_cbor::decode::FromCbor;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = hardy_bpv7::prelude::ValidBundle::from_cbor(data);
});
