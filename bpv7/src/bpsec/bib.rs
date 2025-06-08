use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum Operation {
    HMAC_SHA2(bib_hmac_sha2::Operation),
    Unrecognised(u64, parse::UnknownOperation),
}

pub struct OperationArgs<'a> {
    pub bpsec_source: &'a Eid,
    pub target: &'a block::Block,
    pub target_number: u64,
    pub target_payload: &'a [u8],
    pub source: &'a block::Block,
    pub source_number: u64,
    pub primary_block: &'a [u8],
}

pub struct OperationResult {
    pub protects_primary_block: bool,
    pub can_sign: bool,
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

    pub fn sign(
        &mut self,
        key: Option<&KeyMaterial>,
        args: OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<(), Error> {
        match self {
            Self::HMAC_SHA2(o) => o.sign(key, args, payload_data),
            Self::Unrecognised(v, _) => Err(Error::UnrecognisedContext(*v)),
        }
    }

    pub fn verify(
        &self,
        key: Option<&KeyMaterial>,
        args: OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<OperationResult, Error> {
        match self {
            Self::HMAC_SHA2(o) => o.verify(key, args, payload_data),
            Self::Unrecognised(..) => Ok(OperationResult {
                protects_primary_block: args.target_number == 0,
                can_sign: false,
            }),
        }
    }

    fn emit_context(&self, encoder: &mut cbor::encode::Encoder, source: &Eid) {
        match self {
            Self::HMAC_SHA2(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(self, array: &mut cbor::encode::Array) {
        match self {
            Self::HMAC_SHA2(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array),
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
}

impl cbor::encode::ToCbor for OperationSet {
    fn to_cbor(self, encoder: &mut hardy_cbor::encode::Encoder) {
        // Ensure we process operations in the same order
        let ops = self
            .operations
            .into_iter()
            .collect::<Vec<(u64, Operation)>>();

        // Targets
        encoder.emit_array(Some(ops.len()), |a| {
            for (t, _) in &ops {
                a.emit(*t);
            }
        });

        // Context
        ops.first().unwrap().1.emit_context(encoder, &self.source);

        // Results
        encoder.emit_array(Some(ops.len()), |a| {
            for (_, op) in ops {
                op.emit_result(a);
            }
        });
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
                parse::UnknownOperation::parse(asb, data).map(|(source, operations)| {
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
