/*
#[cfg(test)]
use hardy_bpv7::prelude::*;

#[cfg(test)]
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

#[test]
fn test() {
    let data = include_bytes!("artifacts/bundle/crash-0c47614714278e9e65c9eef00f256397abcbc358");

    eprintln!("Original: {:02x?}", &data);

    if let Ok(ValidBundle::Rewritten(_, data, _)) = ValidBundle::parse(data, get_keys) {
        eprintln!("Rewrite: {:02x?}", &data);

        match ValidBundle::parse(&data, get_keys) {
            Ok(ValidBundle::Valid(..)) => {}
            Ok(ValidBundle::Rewritten(..)) => panic!("Rewrite produced non-canonical results"),
            Ok(ValidBundle::Invalid(_, _, e)) => panic!("Rewrite produced invalid results: {e}"),
            Err(_) => panic!("Rewrite errored"),
        };
    }
}
*/
