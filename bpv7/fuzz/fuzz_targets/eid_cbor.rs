#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok((eid, shortest, len)) =
        hardy_cbor::decode::parse::<(hardy_bpv7::eid::Eid, bool, usize)>(data)
    {
        _ = format!("{eid:?}");
        _ = format!("{eid}");

        // Any parsed EID must re-emit to something that parses back equal.
        let emitted = hardy_cbor::encode::emit(&eid).0;
        let eid2 = hardy_cbor::decode::parse::<hardy_bpv7::eid::Eid>(&emitted)
            .expect("Failed to re-parse emitted EID");
        assert_eq!(eid2, eid, "re-emit/re-parse changed the EID");

        // Unknown-scheme EIDs stash the raw SSP bytes, so a canonical input
        // must re-emit byte-identically (a relay must not corrupt EIDs with
        // future scheme codes).
        if shortest && matches!(eid, hardy_bpv7::eid::Eid::Unknown { .. }) {
            assert_eq!(
                emitted,
                &data[..len],
                "canonical unknown-scheme EID did not round-trip byte-identically"
            );
        }
    }
});
