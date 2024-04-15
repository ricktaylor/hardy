use super::*;

#[derive(Copy, Clone, Default)]
pub struct CreationTimestamp {
    pub creation_time: u64,
    pub sequence_number: u64,
}

impl cbor::encode::ToCbor for &CreationTimestamp {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        cbor::encode::emit_with_tags(
            [
                cbor::encode::emit(self.creation_time),
                cbor::encode::emit(self.sequence_number),
            ],
            tags,
        )
    }
}

impl cbor::decode::FromCbor for CreationTimestamp {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        cbor::decode::parse_value(data, |value, tags| {
            if let cbor::decode::Value::Array(mut a) = value {
                Ok((
                    CreationTimestamp {
                        creation_time: a.parse()?,
                        sequence_number: a.parse()?,
                    },
                    tags.to_vec(),
                ))
            } else {
                Err(anyhow!("Bundle creation timestamp must be a CBOR array"))
            }
        })
        .map(|((t, tags), len)| (t, len, tags))
    }
}
