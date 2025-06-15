use super::*;
use std::ops::Range;

pub mod bcb_aes_gcm;
pub mod bib_hmac_sha2;

#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
struct ScopeFlags {
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

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
            o.map(|(value, shortest, len)| {
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
        })
    }
}

impl hardy_cbor::encode::ToCbor for &ScopeFlags {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
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
        encoder.emit(flags)
    }
}

fn unwrap_key<'a>(
    source: &eid::Eid,
    key_f: impl Fn(&eid::Eid, bpsec::key::Operation) -> Result<Option<&'a bpsec::Key>, bpsec::Error>,
    cek: &[u8],
) -> Result<Zeroizing<Box<[u8]>>, bpsec::Error> {
    let Some(jwk) = key_f(source, key::Operation::UnwrapKey)? else {
        return Err(Error::NoKey(source.clone()));
    };

    let Some(algorithm) = &jwk.key_algorithm else {
        return Err(bpsec::Error::NoKey(source.clone()));
    };

    let key::Type::OctetSequence { key: kek } = &jwk.key_type else {
        return Err(bpsec::Error::NoKey(source.clone()));
    };

    match algorithm {
        key::KeyAlgorithm::A128KW => aes_kw::KekAes128::try_from(kek.as_ref())
            .and_then(|kek| kek.unwrap_vec(cek))
            .map(|v| Zeroizing::from(Box::from(v)))
            .map_field_err("wrapped key"),
        key::KeyAlgorithm::A192KW => aes_kw::KekAes192::try_from(kek.as_ref())
            .and_then(|kek| kek.unwrap_vec(cek))
            .map(|v| Zeroizing::from(Box::from(v)))
            .map_field_err("wrapped key"),
        key::KeyAlgorithm::A256KW => aes_kw::KekAes256::try_from(kek.as_ref())
            .and_then(|kek| kek.unwrap_vec(cek))
            .map(|v| Zeroizing::from(Box::from(v)))
            .map_field_err("wrapped key"),
        _ => Err(bpsec::Error::NoKey(source.clone())),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use serde_json::json;

    fn do_test(data: &[u8], keys: Vec<bpsec::key::Key>) {
        match bundle::ValidBundle::parse(data, |source, op| {
            for k in &keys {
                if let (Some(kid), Some(ops)) = (&k.id, &k.operations) {
                    if let Ok(eid) = kid.parse::<eid::Eid>() {
                        if &eid == source && ops.contains(&op) {
                            return Ok(Some(k));
                        }
                    }
                }
            }
            panic!("Missing test key!")
        })
        .expect("Failed to parse")
        {
            bundle::ValidBundle::Valid(..) => {}
            bundle::ValidBundle::Rewritten(..) => panic!("Non-canonical bundle"),
            bundle::ValidBundle::Invalid(_, _, e) => panic!("Invalid bundle: {e}"),
        }
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
            serde_json::from_value(json!([
                {
                    "kid": "ipn:2.1",
                    "alg": "HS512",
                    "key_ops": ["verify"],
                    "kty": "oct",
                    "k": "GisaKxorGisaKxorGisaKw",
                }
            ]))
            .unwrap(),
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
            serde_json::from_value(json!([
                {
                    "kid": "ipn:2.1",
                    "alg": "A128KW",
                    "key_ops": ["unwrapKey"],
                    "kty": "oct",
                    "k": "YWJjZGVmZ2hpamtsbW5vcA",
                }
            ]))
            .unwrap(),
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
            serde_json::from_value(json!([
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
                }
            ]))
            .unwrap(),
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
            serde_json::from_value(json!([
                {
                    "kid": "ipn:2.1",
                    "alg": "HS384",
                    "key_ops": ["verify"],
                    "kty": "oct",
                    "k": "GisaKxorGisaKxorGisaKw",
                },
                {
                    "kid": "ipn:2.1",
                    "enc": "A256GCM",
                    "key_ops": ["decrypt"],
                    "kty": "oct",
                    "k": "cXdlcnR5dWlvcGFzZGZnaHF3ZXJ0eXVpb3Bhc2RmZ2g",
                }
            ]))
            .unwrap(),
        )
    }
}
