#![no_main]

use hardy_bpv7::prelude::*;
use libfuzzer_sys::fuzz_target;

fn get_keys(
    source: &Eid,
    context: bpsec::Context,
) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error> {
    let keys: &[(EidPattern, bpsec::Context, &'static [u8])] = &[
        (
            "ipn:3.0".parse().unwrap(),
            bpsec::Context::BIB_HMAC_SHA2,
            &hex_literal::hex!("1a2b1a2b1a2b1a2b1a2b1a2b1a2b1a2b"),
        ),
        (
            "ipn:2.1".parse().unwrap(),
            bpsec::Context::BCB_AES_GCM,
            &hex_literal::hex!("71776572747975696f70617364666768"),
        ),
    ];

    for (eid, c2, key) in keys {
        if &context == c2 && eid.is_match(source) {
            return Ok(Some(bpsec::KeyMaterial::SymmetricKey(Box::from(*key))));
        }
    }
    Ok(None)
}

fuzz_target!(|data: &[u8]| {
    if let Ok(ValidBundle::Rewritten(_, data, _)) = ValidBundle::parse(data, get_keys) {
        let Ok(ValidBundle::Valid(..)) = ValidBundle::parse(&data, get_keys) else {
            panic!("Rewrite borked");
        };
    }
});

// cargo cov -- export --format=lcov  -instr-profile ./fuzz/coverage/bundle/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/bundle -ignore-filename-regex='/.cargo/|rustc/|/target/' > ./fuzz/coverage/bundle/lcov.info
// cargo cov -- show --format=html  -instr-profile ./fuzz/coverage/bundle/coverage.profdata ./target/x86_64-unknown-linux-gnu/coverage/x86_64-unknown-linux-gnu/release/bundle -o ./fuzz/coverage/bundle/ -ignore-filename-regex='/.cargo/|rustc/|/target/'
