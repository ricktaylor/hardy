use super::*;
use core::ops::Range;

pub(crate) mod bcb_aes_gcm;
pub(crate) mod bib_hmac_sha2;

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ScopeFlags {
    pub include_primary_block: bool,
    pub include_target_header: bool,
    pub include_security_header: bool,
    pub unrecognised: Option<u64>,
}

impl ScopeFlags {
    pub const NONE: Self = Self {
        include_primary_block: false,
        include_target_header: false,
        include_security_header: false,
        unrecognised: None,
    };
}

impl Default for ScopeFlags {
    fn default() -> Self {
        Self {
            include_primary_block: true,
            include_target_header: true,
            include_security_header: true,
            unrecognised: None,
        }
    }
}

impl hardy_cbor::decode::FromCbor for ScopeFlags {
    type Error = hardy_cbor::decode::Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse::<(u64, bool, usize)>(data).map(|(value, shortest, len)| {
            let mut flags = Self {
                include_primary_block: false,
                include_target_header: false,
                include_security_header: false,
                unrecognised: None,
            };
            let mut unrecognised = value;

            if (value & (1 << 0)) != 0 {
                flags.include_primary_block = true;
                unrecognised &= !(1 << 0);
            }
            if (value & (1 << 1)) != 0 {
                flags.include_target_header = true;
                unrecognised &= !(1 << 1);
            }
            if (value & (1 << 2)) != 0 {
                flags.include_security_header = true;
                unrecognised &= !(1 << 2);
            }

            if unrecognised != 0 {
                flags.unrecognised = Some(unrecognised);
            }
            (flags, shortest, len)
        })
    }
}

impl hardy_cbor::encode::ToCbor for ScopeFlags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        let mut flags = self.unrecognised.unwrap_or(0);
        if self.include_primary_block {
            flags |= 1 << 0;
        }
        if self.include_target_header {
            flags |= 1 << 1;
        }
        if self.include_security_header {
            flags |= 1 << 2;
        }
        encoder.emit(&flags)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use base64::prelude::*;

    #[test]
    fn rfc9173_appendix_a_1() {
        // Note: I've tweaked the creation timestamp to be valid, and added a CRC
        let data = hex_literal::hex!(
            "9f89070001820282010282028202018202820201820118281a000f424042e4fe850b0200
                005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
                8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
                f1a73e303dcd4b6ccece003e95e8164dcc89a156e185010100005823526561647920
                746f2067656e657261746520612033322d62797465207061796c6f6164ff"
        );
        let keys = key::KeySet::new(vec![
            serde_json::from_str::<key::Key>(
                &serde_json::json!({
                            "kid": "ipn:2.1",
                            "kty": "oct",
                            "alg": "HS512",
                            "key_ops": ["verify"],
                            "k": "GisaKxorGisaKxorGisaKw"
                })
                .to_string(),
            )
            .unwrap(),
        ]);

        bundle::ParsedBundle::parse(&data, &keys)
            .unwrap()
            .bundle
            .verify_block(1, &data, &keys)
            .expect("Failed to verify");
    }

    #[test]
    fn rfc9173_appendix_a_2() {
        // Note: I've tweaked the creation timestamp to be valid, and added a CRC
        let data = hex_literal::hex!(
            "9f89070001820282010282028202018202820201820118281a000f424042e4fe850c0201
                0058508101020182028202018482014c5477656c7665313231323132820201820358
                1869c411276fecddc4780df42c8a2af89296fabf34d7fae7008204008181820150ef
                a4b5ac0108e3816c5606479801bc04850101000058233a09c1e63fe23a7f66a59c73
                03837241e070b02619fc59c5214a22f08cd70795e73e9aff"
        );
        let keys = key::KeySet::new(vec![key::Key {
            id: Some("ipn:2.1".into()),
            key_algorithm: Some(key::KeyAlgorithm::A128KW),
            enc_algorithm: Some(key::EncAlgorithm::A128GCM),
            operations: Some([key::Operation::UnwrapKey, key::Operation::Decrypt].into()),
            key_type: key::Type::OctetSequence {
                key: BASE64_URL_SAFE_NO_PAD
                    .decode(b"YWJjZGVmZ2hpamtsbW5vcA")
                    .unwrap()
                    .into(),
            },
            ..Default::default()
        }]);

        bundle::ParsedBundle::parse(&data, &keys)
            .unwrap()
            .bundle
            .decrypt_block_data(1, &data, &keys)
            .expect("Failed to decrypt");
    }

    #[test]
    fn rfc9173_appendix_a_3() {
        let data = hex_literal::hex!(
            "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                00585c8200020101820282030082820105820300828182015820cac6ce8e4c5dae57
                988b757e49a6dd1431dc04763541b2845098265bc817241b81820158203ed614c0d9
                7f49b3633627779aa18a338d212bf3c92b97759d9739cd50725596850c0401005834
                8101020182028202018382014c5477656c7665313231323132820201820400818182
                0150efa4b5ac0108e3816c5606479801bc0485070200004319012c85010100005823
                3a09c1e63fe23a7f66a59c7303837241e070b02619fc59c5214a22f08cd70795e73e
                9aff"
        );
        let keys = key::KeySet::new(vec![
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
        ]);

        let bundle = bundle::ParsedBundle::parse(&data, &keys).unwrap().bundle;
        bundle
            .verify_block(2, &data, &keys)
            .expect("Failed to verify");
        bundle
            .verify_block(0, &data, &keys)
            .expect("Failed to verify");
        bundle
            .decrypt_block_data(1, &data, &keys)
            .expect("Failed to decrypt");
    }

    /*

    The example bundle is invalid as it lacks a CRC on the Primary Block

    #[test]
    fn rfc9173_appendix_a_4() {
        let data = hex_literal::hex!(
            // I have added a bundle age block
            "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
                420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
                9b88029191850c0201005849820301020182028202018382014c5477656c76653132
                313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
                50d2c51cb2481792dae8b21d848cede99b850704000041018501010000582390eab6
                457593379298a8724e16e61f837488e127212b59ac91f8a86287b7d07630a122ff"
        );
        let keys = key::KeySet::new(vec![
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
        ]);

        let bundle = bundle::ParsedBundle::parse(&data, &keys).unwrap().bundle;
        bundle
            .decrypt_block_data(1, &data, &keys)
            .expect("Failed to decrypt");
        bundle
            .verify_block(1, &data, &keys)
            .expect("Failed to verify");
    }*/

    // TODO: Implement test for Wrapped Key Unwrap (LLR 2.2.4, 2.2.7).
    // Scenario: Verify unwrapping of a session key using a KEK.

    // TODO: Implement test for Wrapped Key Fail.
    // Scenario: Verify failure when unwrapping a corrupted key blob.

    #[test]
    fn test_sign_then_encrypt() {
        use crate::bpsec::{encryptor, key, signer};
        use crate::builder::Builder;
        use crate::bundle;
        use crate::creation_timestamp::CreationTimestamp;

        // 1. Create a bundle
        let (bundle, bundle_bytes) =
            Builder::new("ipn:1.2".parse().unwrap(), "ipn:2.1".parse().unwrap())
                .with_report_to("ipn:2.1".parse().unwrap())
                .with_lifetime(core::time::Duration::from_millis(1000))
                .with_payload(b"hello".as_slice().into())
                .build(CreationTimestamp::now())
                .unwrap();

        // Keys
        let sign_key: key::Key = serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "HS256",
            "key_ops": ["sign", "verify"],
            "k": "c2VjcmV0X3NpZ25pbmdfa2V5"
        }))
        .unwrap();
        let enc_key: key::Key = serde_json::from_value(serde_json::json!({
            "kid": "ipn:2.1",
            "kty": "oct",
            "alg": "A128KW",
            "enc": "A128GCM",
            "key_ops": ["encrypt", "decrypt", "wrapKey", "unwrapKey"],
            "k": "AAAAAAAAAAAAAAAAAAAAAA"
        }))
        .unwrap();
        let sign_keys = key::KeySet::new(vec![sign_key.clone()]);
        let enc_keys = key::KeySet::new(vec![enc_key.clone()]);
        let all_keys = key::KeySet::new(vec![sign_key.clone(), enc_key.clone()]);

        // 2. Sign
        let signer = signer::Signer::new(&bundle, &bundle_bytes);
        let signer = signer
            .sign_block(
                1,
                signer::Context::HMAC_SHA2(ScopeFlags::default()),
                "ipn:2.1".parse().unwrap(),
                sign_key.clone(),
            )
            .expect("Failed to sign block");
        let signed_bytes = signer.rebuild().expect("Failed to rebuild signed bundle");
        // println!("Bundle bytes: {:02x?}", signed_bytes);

        let parsed_signed = bundle::ParsedBundle::parse(&signed_bytes, &sign_keys)
            .expect("Failed to parse signed bundle");

        // 3. Encrypt
        // Exclude the security header from AAD to avoid mismatches due to BCB header mutation
        let flags = ScopeFlags {
            include_security_header: false,
            ..ScopeFlags::default()
        };

        let encryptor = encryptor::Encryptor::new(&parsed_signed.bundle, &signed_bytes);
        let encryptor = encryptor
            .encrypt_block(
                1,
                encryptor::Context::AES_GCM(flags),
                "ipn:2.1".parse().unwrap(),
                enc_key.clone(),
            )
            .expect("Failed to encrypt block");
        let encrypted_bytes = encryptor
            .rebuild()
            .expect("Failed to rebuild encrypted bundle");
        // println!("Bundle bytes: {:02x?}", encrypted_bytes);

        // 4. Decrypt and Verify
        let parsed_enc = bundle::ParsedBundle::parse(&encrypted_bytes, &enc_keys)
            .expect("Failed to parse encrypted bundle");
        // println!("{:#?}", parsed_enc);

        // Attempt to decrypt the BIB first to isolate decryption issues from verification issues
        if let Some(bib_num) = parsed_enc.bundle.blocks.get(&1).and_then(|b| b.bib) {
            // println!("Found BIB at block {bib_num}");
            parsed_enc
                .bundle
                .decrypt_block_data(bib_num, &encrypted_bytes, &enc_keys)
                .expect("BIB Decryption failed");
        }

        // This should succeed if everything is working
        parsed_enc
            .bundle
            .verify_block(1, &encrypted_bytes, &all_keys)
            .expect("Verification failed");

        // Also check decryption of payload directly
        let payload = parsed_enc
            .bundle
            .decrypt_block_data(1, &encrypted_bytes, &enc_keys)
            .expect("Decryption failed");
        assert_eq!(payload.as_ref(), b"hello");
    }
}
