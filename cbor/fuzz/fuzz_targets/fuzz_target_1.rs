#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // fuzzed code goes here
    let _ = hardy_cbor::decode::try_parse_value(data, |_,_|{Ok(())});
});
