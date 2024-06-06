#![no_main]

use libfuzzer_sys::fuzz_target;
use hardy_cbor::decode::FromCbor;

fuzz_target!(|data: &[u8]| {
    let _ =
        hardy_bpa_core::bundle::Eid::from_cbor(data);
});
