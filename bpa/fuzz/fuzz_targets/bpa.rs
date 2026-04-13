#![no_main]

use libfuzzer_sys::{Corpus, fuzz_target};

fuzz_target!(|data: &[u8]| -> Corpus {
    if hardy_bpa_fuzz::send_random(data) {
        Corpus::Keep
    } else {
        Corpus::Reject
    }
});
