use hardy_bpv7::{bpsec, bundle::ValidBundle, eid::Eid};
use serde_json::json;

struct Keys(Vec<bpsec::key::Key>);

impl bpsec::key::KeyStore for Keys {
    fn decrypt_keys<'a>(
        &'a self,
        source: &Eid,
        operation: &[bpsec::key::Operation],
    ) -> impl Iterator<Item = &'a bpsec::key::Key> {
        self.0.iter().filter(move |k| {
            if let (Some(kid), Some(ops)) = (&k.id, &k.operations) {
                if let Ok(eid) = kid.parse::<Eid>() {
                    if &eid == source {
                        for op in operation {
                            if !ops.contains(op) {
                                return false;
                            }
                        }
                        return true;
                    }
                }
            }
            false
        })
    }
}

pub fn test_bundle(orig_data: &[u8]) {
    static KEYS: std::sync::OnceLock<Keys> = std::sync::OnceLock::new();

    let keys = KEYS.get_or_init(|| {
        Keys(
            serde_json::from_value(json!([
                {
                    "kid": "ipn:2.1",
                    "alg": "HS512",
                    "key_ops": ["verify"],
                    "kty": "oct",
                    "k": "GisaKxorGisaKxorGisaKw",
                },
                {
                    "kid": "ipn:2.1",
                    "alg": "A128KW",
                    "key_ops": ["unwrapKey","decrypt"],
                    "kty": "oct",
                    "k": "YWJjZGVmZ2hpamtsbW5vcA",
                },
                {
                    "kid": "ipn:3.0",
                    "alg": "HS256",
                    "key_ops": ["verify"],
                    "kty": "oct",
                    "k": "GisaKxorGisaKxorGisaKw",
                },
                {
                    "kid": "ipn:2.1",
                    "alg": "dir",
                    "enc": "A128GCM",
                    "key_ops": ["decrypt"],
                    "kty": "oct",
                    "k": "cXdlcnR5dWlvcGFzZGZnaA",
                },
                {
                    "kid": "ipn:2.1",
                    "alg": "HS384",
                    "key_ops": ["verify"],
                    "kty": "oct",
                    "k": "GisaKxorGisaKxorGisaKw",
                },
                {
                    "kid": "ipn:3.1",
                    "enc": "A256GCM",
                    "key_ops": ["decrypt"],
                    "kty": "oct",
                    "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g",
                }
            ]))
            .unwrap(),
        )
    });

    if let Ok(ValidBundle::Rewritten(_, rewritten_data, _)) = ValidBundle::parse(orig_data, keys) {
        match ValidBundle::parse(&rewritten_data, keys) {
            Ok(ValidBundle::Valid(..)) => {}
            Ok(ValidBundle::Rewritten(..)) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {rewritten_data:02x?}");
                panic!("Rewrite produced non-canonical results")
            }
            Ok(ValidBundle::Invalid(_, _, e)) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {rewritten_data:02x?}");
                panic!("Rewrite produced invalid results: {e}")
            }
            Err(_) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {rewritten_data:02x?}");
                panic!("Rewrite errored");
            }
        };
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::io::Read;

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
}
