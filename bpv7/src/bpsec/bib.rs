use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum Operation {
    HMAC_SHA2(bib_hmac_sha2::Operation),
    Unrecognised(u64, parse::UnknownOperation),
}

impl Operation {
    pub fn context_id(&self) -> Context {
        match self {
            Self::HMAC_SHA2(_) => Context::BIB_HMAC_SHA2,
            Self::Unrecognised(id, _) => Context::Unrecognised(*id),
        }
    }

    pub fn is_unsupported(&self) -> bool {
        match self {
            Operation::HMAC_SHA2(operation) => operation.is_unsupported(),
            Operation::Unrecognised(..) => true,
        }
    }

    pub fn verify(
        &self,
        key: Option<&KeyMaterial>,
        args: OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<bool, Error> {
        match self {
            Self::HMAC_SHA2(o) => o.verify(key, args, payload_data),
            Self::Unrecognised(..) => Ok(args.target_number == 0),
        }
    }

    fn emit_context(&self, encoder: &mut cbor::encode::Encoder, source: &Eid, source_data: &[u8]) {
        match self {
            Self::HMAC_SHA2(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id, source_data),
        }
    }

    fn emit_result(self, array: &mut cbor::encode::Array, source_data: &[u8]) {
        match self {
            Self::HMAC_SHA2(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array, source_data),
        }
    }
}

pub struct OperationSet {
    pub source: Eid,
    pub operations: HashMap<u64, Operation>,
}

impl OperationSet {
    pub fn is_unsupported(&self) -> bool {
        self.operations.values().next().unwrap().is_unsupported()
    }

    pub fn rewrite(
        block: &Block,
        blocks_to_remove: &HashSet<u64>,
        source_data: &[u8],
    ) -> Result<Vec<u8>, bpsec::Error> {
        cbor::decode::parse_value(block.payload(source_data), |v, _, _| match v {
            cbor::decode::Value::Bytes(data) => cbor::decode::parse::<OperationSet>(data)
                .map(|op| op.rewrite_payload(blocks_to_remove, data)),
            cbor::decode::Value::ByteStream(data) => {
                let data = data.iter().fold(Vec::new(), |mut data, d| {
                    data.extend(*d);
                    data
                });
                cbor::decode::parse::<OperationSet>(&data)
                    .map(|op| op.rewrite_payload(blocks_to_remove, &data))
            }
            _ => unreachable!(),
        })
        .map(|v| v.0)
    }

    fn rewrite_payload(self, blocks_to_remove: &HashSet<u64>, payload_data: &[u8]) -> Vec<u8> {
        let mut encoder = cbor::encode::Encoder::new();

        // Ensure we process operations in the same order
        let ops = self
            .operations
            .into_iter()
            .filter(|v| !blocks_to_remove.contains(&v.0))
            .collect::<Vec<(u64, Operation)>>();

        // Targets
        encoder.emit_array(Some(ops.len()), |a| {
            for (t, _) in &ops {
                a.emit(*t);
            }
        });

        // Context
        ops.first()
            .unwrap()
            .1
            .emit_context(&mut encoder, &self.source, payload_data);

        // Results
        encoder.emit_array(Some(ops.len()), |a| {
            for (_, op) in ops {
                op.emit_result(a, payload_data);
            }
        });

        encoder.build()
    }
}

impl cbor::decode::FromCbor for OperationSet {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        let Some((asb, mut shortest, len)) =
            cbor::decode::try_parse::<(parse::AbstractSyntaxBlock, bool, usize)>(data)?
        else {
            return Ok(None);
        };

        // Unpack into strong types
        match asb.context {
            Context::BIB_HMAC_SHA2 => {
                bib_hmac_sha2::parse(asb, data, &mut shortest).map(|(source, operations)| {
                    Some((OperationSet { source, operations }, shortest, len))
                })
            }
            Context::Unrecognised(id) => {
                parse::UnknownOperation::parse(asb).map(|(source, operations)| {
                    Some((
                        OperationSet {
                            source,
                            operations: operations
                                .into_iter()
                                .map(|(t, o)| (t, Operation::Unrecognised(id, o)))
                                .collect(),
                        },
                        shortest,
                        len,
                    ))
                })
            }
            c => Err(Error::InvalidContext(c)),
        }
    }
}
