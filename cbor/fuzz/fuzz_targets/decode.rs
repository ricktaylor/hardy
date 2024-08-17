#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = hardy_cbor::decode::try_parse_value(data, |value, _| {
        format!("{value:?}");
        Ok::<_, hardy_cbor::decode::Error>(())
    });
});
