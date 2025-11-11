use super::*;
use core::ops::Range;
use rand::{TryRngCore, rngs::OsRng};

pub(crate) mod bcb_aes_gcm;
pub(crate) mod bib_hmac_sha2;

#[derive(Debug, Hash, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ScopeFlags {
    include_primary_block: bool,
    include_target_header: bool,
    include_security_header: bool,
    unrecognised: u64,
}

impl Default for ScopeFlags {
    fn default() -> Self {
        Self {
            include_primary_block: true,
            include_target_header: true,
            include_security_header: true,
            unrecognised: 0,
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
                unrecognised: value & !7,
            };
            for b in 0..=2 {
                if value & (1 << b) != 0 {
                    match b {
                        0 => flags.include_primary_block = true,
                        1 => flags.include_target_header = true,
                        2 => flags.include_security_header = true,
                        _ => unreachable!(),
                    }
                }
            }
            (flags, shortest, len)
        })
    }
}

impl hardy_cbor::encode::ToCbor for ScopeFlags {
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        let mut flags = self.unrecognised;
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

fn rand_key(mut cek: Box<[u8]>) -> Result<zeroize::Zeroizing<Box<[u8]>>, Error> {
    OsRng
        .try_fill_bytes(&mut cek)
        .map_err(|e| Error::Algorithm(e.to_string()))?;
    Ok(zeroize::Zeroizing::from(cek))
}

#[cfg(test)]
mod test {
    use super::*;
    use base64::prelude::*;

    struct Keys<'a>(&'a [key::Key]);

    impl<'b> key::KeyStore for Keys<'b> {
        fn decrypt_keys<'a>(
            &'a self,
            source: &eid::Eid,
            operation: &[key::Operation],
        ) -> impl Iterator<Item = &'a key::Key> {
            self.0.iter().filter(move |k| {
                if let (Some(kid), Some(ops)) = (&k.id, &k.operations)
                    && let Ok(eid) = kid.parse::<eid::Eid>()
                    && &eid == source
                {
                    for op in operation {
                        if !ops.contains(op) {
                            return false;
                        }
                    }
                    true
                } else {
                    false
                }
            })
        }
    }

    fn do_test(data: &[u8], keys: &[key::Key]) {
        bundle::ParsedBundle::parse(data, &Keys(keys)).expect("Failed to parse");
    }

    #[test]
    fn rfc9173_appendix_a_1() {
        do_test(
            // Note: I've tweaked the creation timestamp to be valid, and added a CRC
            &hex_literal::hex!(
                "9f89070001820282010282028202018202820201820118281a000f424042e4fe850b0200
                005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
                8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
                f1a73e303dcd4b6ccece003e95e8164dcc89a156e185010100005823526561647920
                746f2067656e657261746520612033322d62797465207061796c6f6164ff"
            ),
            &[key::Key {
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
            }],
        )
    }

    #[test]
    fn rfc9173_appendix_a_2() {
        do_test(
            // Note: I've tweaked the creation timestamp to be valid, and added a CRC
            &hex_literal::hex!(
                "9f89070001820282010282028202018202820201820118281a000f424042e4fe850c0201
                0058508101020182028202018482014c5477656c7665313231323132820201820358
                1869c411276fecddc4780df42c8a2af89296fabf34d7fae7008204008181820150ef
                a4b5ac0108e3816c5606479801bc04850101000058233a09c1e63fe23a7f66a59c73
                03837241e070b02619fc59c5214a22f08cd70795e73e9aff"
            ),
            &[key::Key {
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
            }],
        )
    }

    #[test]
    fn rfc9173_appendix_a_3() {
        do_test(
            &hex_literal::hex!(
                "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                00585c8200020101820282030082820105820300828182015820cac6ce8e4c5dae57
                988b757e49a6dd1431dc04763541b2845098265bc817241b81820158203ed614c0d9
                7f49b3633627779aa18a338d212bf3c92b97759d9739cd50725596850c0401005834
                8101020182028202018382014c5477656c7665313231323132820201820400818182
                0150efa4b5ac0108e3816c5606479801bc0485070200004319012c85010100005823
                3a09c1e63fe23a7f66a59c7303837241e070b02619fc59c5214a22f08cd70795e73e
                9aff"
            ),
            &[
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
            ],
        )
    }

    #[test]
    fn rfc9173_appendix_a_4() {
        do_test(
            &hex_literal::hex!(
                // I have added a bundle age block
                "9f88070000820282010282028202018202820201820018281a000f4240850b0300
                005846438ed6208eb1c1ffb94d952175167df0902902064a2983910c4fb2340790bf
                420a7d1921d5bf7c4721e02ab87a93ab1e0b75cf62e4948727c8b5dae46ed2af0543
                9b88029191850c0201005849820301020182028202018382014c5477656c76653132
                313231328202038204078281820150220ffc45c8a901999ecc60991dd78b29818201
                50d2c51cb2481792dae8b21d848cede99b850704000041018501010000582390eab6
                457593379298a8724e16e61f837488e127212b59ac91f8a86287b7d07630a122ff"
            ),
            &[
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
            ],
        )
    }
}
