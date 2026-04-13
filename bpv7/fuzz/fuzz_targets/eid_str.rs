#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(s) = std::str::from_utf8(data)
        && let Ok(eid) = s.parse::<hardy_bpv7::eid::Eid>()
    {
        let s2 = eid.to_string();
        let eid2 = s2
            .parse::<hardy_bpv7::eid::Eid>()
            .expect("Failed to round-trip");

        if eid2 != eid {
            panic!("{s} and {s2}");
        }
        assert_eq!(eid2, eid);
    }
});
