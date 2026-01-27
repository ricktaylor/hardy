use hardy_bpv7::{bpsec::key, bundle::RewrittenBundle};
use serde_json::json;

pub fn test_bundle(orig_data: &[u8]) {
    static KEYS: std::sync::OnceLock<key::KeySet> = std::sync::OnceLock::new();

    let keys = KEYS.get_or_init(|| {
        serde_json::from_value(json!({
            "keys": [
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "HS512",
                    "key_ops": ["verify"],
                    "k": "GisaKxorGisaKxorGisaKw"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "A128KW",
                    "key_ops": ["unwrapKey", "decrypt"],
                    "k": "YWJjZGVmZ2hpamtsbW5vcA"
                },
                {
                    "kid": "ipn:3.0",
                    "kty": "oct",
                    "alg": "HS256",
                    "key_ops": ["verify"],
                    "k": "GisaKxorGisaKxorGisaKw"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "dir",
                    "enc": "A128GCM",
                    "key_ops": ["decrypt"],
                    "k": "cXdlcnR5dWlvcGFzZGZnaA"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "alg": "HS384",
                    "key_ops": ["verify"],
                    "k": "GisaKxorGisaKxorGisaKw"
                },
                {
                    "kid": "ipn:2.1",
                    "kty": "oct",
                    "enc": "A256GCM",
                    "key_ops": ["decrypt"],
                    "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g"
                }
            ]
        }))
        .unwrap()
    });

    if let Ok(RewrittenBundle::Rewritten { new_data, .. }) =
        RewrittenBundle::parse_with_keys(orig_data, keys)
    {
        match RewrittenBundle::parse_with_keys(&new_data, keys) {
            Ok(RewrittenBundle::Valid { .. }) => {}
            Ok(RewrittenBundle::Rewritten { .. }) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {new_data:02x?}");
                panic!("Rewrite produced non-canonical results")
            }
            Ok(RewrittenBundle::Invalid { error, .. }) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {new_data:02x?}");
                panic!("Rewrite produced invalid results: {error}")
            }
            Err(_) => {
                eprintln!("Original: {orig_data:02x?}");
                eprintln!("Rewrite: {new_data:02x?}");
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
                    std::env::current_dir().unwrap().display()
                );
            }
            Ok(dir) => {
                for entry in dir.flatten() {
                    let path = entry.path();
                    if path.is_file()
                        && let Ok(mut file) = std::fs::File::open(&path)
                    {
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
