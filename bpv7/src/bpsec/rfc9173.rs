use super::*;

#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ScopeFlags {
    pub include_primary_block: bool,
    pub include_target_header: bool,
    pub include_security_header: bool,
    pub unrecognised: u64,
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

impl cbor::decode::FromCbor for ScopeFlags {
    type Error = cbor::decode::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse::<(u64, bool, usize)>(data).map(|o| {
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

impl cbor::encode::ToCbor for &ScopeFlags {
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

pub fn unwrap_key(
    key_material: &KeyMaterial,
    wrapped_key: &Option<Box<[u8]>>,
) -> Result<Zeroizing<Box<[u8]>>, aes_kw::Error> {
    let KeyMaterial::SymmetricKey(key_material) = key_material else {
        return Err(aes_kw::Error::InvalidKekSize { size: 0 });
    };

    let Some(wrapped_key) = wrapped_key else {
        return Ok(Zeroizing::new(key_material.clone()));
    };

    // KeyWrap!
    match key_material.len() {
        16 => aes_kw::KekAes128::new(key_material.as_ref().into())
            .unwrap_vec(wrapped_key)
            .map(|v| Zeroizing::from(Box::from(v))),
        24 => aes_kw::KekAes192::new(key_material.as_ref().into())
            .unwrap_vec(wrapped_key)
            .map(|v| Zeroizing::from(Box::from(v))),
        _ => aes_kw::KekAes256::new(key_material.as_ref().into())
            .unwrap_vec(wrapped_key)
            .map(|v| Zeroizing::from(Box::from(v))),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    fn do_test(data: &[u8], keys: &[(Eid, Box<[u8]>)]) {
        match ValidBundle::parse(data, |source| {
            for (eid, key) in keys {
                if eid == source {
                    return Ok(Some(bpsec::KeyMaterial::SymmetricKey(key.clone())));
                }
            }
            Err(bpsec::Error::NoKey(source.clone()))
        })
        .expect("Failed to parse")
        {
            ValidBundle::Valid(..) => {}
            ValidBundle::Rewritten(..) => panic!("Non-canonical bundle"),
            ValidBundle::Invalid(_, _, e) => panic!("Invalid bundle: {e}"),
        }
    }

    #[test]
    fn rfc9173_appendix_a_1() {
        do_test(
            // Note: I've tweaked the creation timestamp to be valid
            &hex_literal::hex!(
                "9f88070000820282010282028202018202820201820118281a000f4240850b0200
                005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
                8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
                f1a73e303dcd4b6ccece003e95e8164dcc89a156e185010100005823526561647920
                746f2067656e657261746520612033322d62797465207061796c6f6164ff"
            ),
            &[(
                Eid::Ipn3 {
                    allocator_id: 0,
                    node_number: 2,
                    service_number: 1,
                },
                hex_literal::hex!("1a2b1a2b1a2b1a2b1a2b1a2b1a2b1a2b").into(),
            )],
        )
    }

    #[test]
    fn rfc9173_appendix_a_2() {
        do_test(
            // Note: I've tweaked the creation timestamp to be valid
            &hex_literal::hex!(
                "9f88070000820282010282028202018202820201820118281a000f4240850c0201
                0058508101020182028202018482014c5477656c7665313231323132820201820358
                1869c411276fecddc4780df42c8a2af89296fabf34d7fae7008204008181820150ef
                a4b5ac0108e3816c5606479801bc04850101000058233a09c1e63fe23a7f66a59c73
                03837241e070b02619fc59c5214a22f08cd70795e73e9aff"
            ),
            &[(
                Eid::Ipn3 {
                    allocator_id: 0,
                    node_number: 2,
                    service_number: 1,
                },
                hex_literal::hex!("6162636465666768696a6b6c6d6e6f70").into(),
            )],
        )
    }
}
