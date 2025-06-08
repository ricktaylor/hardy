#![cfg(test)]

use hardy_bpv7::{bpsec, bundle::ValidBundle, eid::Eid};
use std::io::Read;

fn get_keys(
    source: &Eid,
    context: bpsec::Context,
) -> Result<Option<bpsec::KeyMaterial>, bpsec::Error> {
    static KEYS: std::sync::OnceLock<
        hardy_eid_pattern::EidPatternMap<(bpsec::Context, &'static [u8])>,
    > = std::sync::OnceLock::new();

    let keys = KEYS.get_or_init(|| {
        let keys: &[(hardy_eid_pattern::EidPattern, bpsec::Context, &'static [u8])] = &[
            (
                "ipn:3.0".parse().unwrap(),
                bpsec::Context::BIB_RFC9173_HMAC_SHA2,
                &hex_literal::hex!("1a2b1a2b1a2b1a2b1a2b1a2b1a2b1a2b"),
            ),
            (
                "ipn:2.1".parse().unwrap(),
                bpsec::Context::BCB_RFC9173_AES_GCM,
                &hex_literal::hex!("71776572747975696f70617364666768"),
            ),
        ];
        let mut map = hardy_eid_pattern::EidPatternMap::new();
        for (eid, c2, key) in keys {
            map.insert(eid.clone(), (*c2, *key));
        }
        map
    });

    for (c2, key) in keys.find(source) {
        if &context == c2 {
            return Ok(Some(bpsec::KeyMaterial::SymmetricKey(Box::from(*key))));
        }
    }
    Ok(None)
}

fn test_bundle(data: &[u8]) {
    eprintln!("Original: {:02x?}", data);

    if let Ok(ValidBundle::Rewritten(_, data, _)) = ValidBundle::parse(data, get_keys) {
        eprintln!("Rewrite: {:02x?}", &data);

        match ValidBundle::parse(&data, get_keys) {
            Ok(ValidBundle::Valid(..)) => {}
            Ok(ValidBundle::Rewritten(..)) => {
                panic!("Rewrite produced non-canonical results")
            }
            Ok(ValidBundle::Invalid(_, _, e)) => {
                panic!("Rewrite produced invalid results: {e}")
            }
            Err(_) => panic!("Rewrite errored"),
        };
    }
}

#[test]
fn test() {
    if let Ok(mut file) =
        std::fs::File::open("./artifacts/bundle/crash-effffdc7a8837e1dc7225d82466f3f068508a79a")
    {
        let mut buffer = Vec::new();
        if file.read_to_end(&mut buffer).is_ok() {
            test_bundle(&buffer);
        }
    }
}

#[test]
fn test_all() {
    match std::fs::read_dir("./corpus/bundle") {
        Err(e) => {
            eprintln!(
                "Failed to open dir: {e}, curr dir: {}",
                std::env::current_dir().unwrap().to_string_lossy()
            );
        }
        Ok(dir) => {
            for entry in dir {
                if let Ok(entry) = entry {
                    let path = entry.path();
                    if path.is_file() {
                        if let Ok(mut file) = std::fs::File::open(&path) {
                            let mut buffer = Vec::new();
                            if file.read_to_end(&mut buffer).is_ok() {
                                test_bundle(&buffer);
                            }
                        }
                    }
                }
            }
        }
    }
}
