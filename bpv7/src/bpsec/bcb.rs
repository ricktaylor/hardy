use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum Operation {
    AES_GCM(bcb_aes_gcm::Operation),
    Unrecognised(u64, parse::UnknownOperation),
}

impl Operation {
    pub fn context_id(&self) -> Context {
        match self {
            Operation::AES_GCM(_) => Context::BCB_AES_GCM,
            Operation::Unrecognised(id, _) => Context::Unrecognised(*id),
        }
    }

    pub fn decrypt(
        &self,
        key: &KeyMaterial,
        bundle: &Bundle,
        data: &[u8],
    ) -> Result<Box<[u8]>, Error> {
        match self {
            Operation::AES_GCM(op) => op.decrypt(key, bundle, data),
            Operation::Unrecognised(_, _) => todo!(),
        }
    }

    fn emit_context(&self, encoder: &mut cbor::encode::Encoder, source: &Eid) -> usize {
        match self {
            Operation::AES_GCM(o) => o.emit_context(encoder, source),
            Operation::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(&self, array: &mut cbor::encode::Array) {
        match self {
            Operation::AES_GCM(o) => o.emit_result(array),
            Operation::Unrecognised(_, o) => o.emit_result(array),
        }
    }
}

pub struct OperationSet {
    pub source: Eid,
    pub operations: HashMap<u64, Operation>,
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
            Context::BCB_AES_GCM => {
                bcb_aes_gcm::parse(asb, data, &mut shortest).map(|(source, operations)| {
                    Some((OperationSet { source, operations }, shortest, len))
                })
            }
            Context::Unrecognised(id) => parse::UnknownOperation::parse(asb, data, &mut shortest)
                .map(|(source, operations)| {
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
                }),
            c => Err(Error::InvalidContext(c)),
        }
    }
}

impl cbor::encode::ToCbor for &OperationSet {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) -> usize {
        // Targets
        let mut len = encoder.emit_array(Some(self.operations.len()), |a, _| {
            for t in self.operations.keys() {
                a.emit(*t);
            }
        });

        // Context
        len += self
            .operations
            .values()
            .next()
            .unwrap()
            .emit_context(encoder, &self.source);

        // Results
        len + encoder.emit_array(Some(self.operations.len()), |a, _| {
            for (idx, op) in self.operations.values().enumerate() {
                a.emit(idx);
                op.emit_result(a);
            }
        })
    }
}
