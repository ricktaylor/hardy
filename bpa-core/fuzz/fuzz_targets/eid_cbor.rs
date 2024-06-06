#![no_main]

use hardy_cbor::decode::FromCbor;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = hardy_bpa_core::bundle::Eid::from_cbor(data);
});
