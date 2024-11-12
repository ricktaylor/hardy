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

    #[test]
    fn rfc9173_appendix_a_1() {
        // Note: I've tweaked the creation timestamp to be valid
        let data = &hex_literal::hex!(
            "9f88070000820282010282028202018202820201820118281a000f4240850b0200
            005856810101018202820201828201078203008181820158403bdc69b3a34a2b5d3a
            8554368bd1e808f606219d2a10a846eae3886ae4ecc83c4ee550fdfb1cc636b904e2
            f1a73e303dcd4b6ccece003e95e8164dcc89a156e185010100005823526561647920
            746f2067656e657261746520612033322d62797465207061796c6f6164ff"
        );

        let ValidBundle::Valid(..) = ValidBundle::parse(data, |_| {
            Ok(Some(bpsec::KeyMaterial::SymmetricKey(Box::new(
                hex_literal::hex!("1a2b1a2b1a2b1a2b1a2b1a2b1a2b1a2b"),
            ))))
        })
        .expect("Failed to parse") else {
            panic!("No!");
        };
    }
}
