use base64::prelude::*;
use hardy_bpv7::{bpsec::key, bundle::RewrittenBundle, eid::Eid};

struct Keys(Vec<key::Key>);

impl key::KeyStore for Keys {
    fn decrypt_keys<'a>(
        &'a self,
        source: &Eid,
        operation: &[key::Operation],
    ) -> impl Iterator<Item = &'a key::Key> {
        self.0.iter().filter(move |k| {
            if let (Some(kid), Some(ops)) = (&k.id, &k.operations)
                && let Ok(eid) = kid.parse::<Eid>()
                && &eid == source
            {
                for op in operation {
                    if !ops.contains(op) {
                        return false;
                    }
                }
                return true;
            }
            false
        })
    }
}

pub fn test_bundle(orig_data: &[u8]) {
    static KEYS: std::sync::OnceLock<Keys> = std::sync::OnceLock::new();

    let keys = KEYS.get_or_init(|| {
        Keys(
            [
                key::Key {
                    id: Some("ipn:2.1".into()),
                    key_algorithm: Some(key::KeyAlgorithm::HS512),
                    operations: Some([key::Operation::Verify].into()),
                    key_type: key::Type::OctetSequence {
                        key: BASE64_URL_SAFE_NO_PAD
                            .decode(b"GisaKxorGisaKxorGisaKw")
                            .unwrap()
                            .into(),
                    },
                    ..Default::default()
                },
                key::Key {
                    id: Some("ipn:2.1".into()),
                    key_algorithm: Some(key::KeyAlgorithm::A128KW),
                    operations: Some([key::Operation::UnwrapKey, key::Operation::Decrypt].into()),
                    key_type: key::Type::OctetSequence {
                        key: BASE64_URL_SAFE_NO_PAD
                            .decode(b"YWJjZGVmZ2hpamtsbW5vcA")
                            .unwrap()
                            .into(),
                    },
                    ..Default::default()
                },
                key::Key {
                    id: Some("ipn:3.0".into()),
                    key_algorithm: Some(key::KeyAlgorithm::HS256),
                    operations: Some([key::Operation::Verify].into()),
                    key_type: key::Type::OctetSequence {
                        key: BASE64_URL_SAFE_NO_PAD
                            .decode(b"GisaKxorGisaKxorGisaKw")
                            .unwrap()
                            .into(),
                    },
                    ..Default::default()
                },
                key::Key {
                    id: Some("ipn:2.1".into()),
                    key_algorithm: Some(key::KeyAlgorithm::Direct),
                    enc_algorithm: Some(key::EncAlgorithm::A128GCM),
                    operations: Some([key::Operation::Decrypt].into()),
                    key_type: key::Type::OctetSequence {
                        key: BASE64_URL_SAFE_NO_PAD
                            .decode(b"cXdlcnR5dWlvcGFzZGZnaA")
                            .unwrap()
                            .into(),
                    },
                    ..Default::default()
                },
                key::Key {
                    id: Some("ipn:2.1".into()),
                    key_algorithm: Some(key::KeyAlgorithm::HS384),
                    operations: Some([key::Operation::Verify].into()),
                    key_type: key::Type::OctetSequence {
                        key: BASE64_URL_SAFE_NO_PAD
                            .decode(b"GisaKxorGisaKxorGisaKw")
                            .unwrap()
                            .into(),
                    },
                    ..Default::default()
                },
                key::Key {
                    id: Some("ipn:2.1".into()),
                    enc_algorithm: Some(key::EncAlgorithm::A256GCM),
                    operations: Some([key::Operation::Decrypt].into()),
                    key_type: key::Type::OctetSequence {
                        key: BASE64_URL_SAFE_NO_PAD
                            .decode(b"cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g")
                            .unwrap()
                            .into(),
                    },
                    ..Default::default()
                },
            ]
            .into(),
        )
    });

    if let Ok(RewrittenBundle::Rewritten { new_data, .. }) = RewrittenBundle::parse(orig_data, keys)
    {
        match RewrittenBundle::parse(&new_data, keys) {
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
