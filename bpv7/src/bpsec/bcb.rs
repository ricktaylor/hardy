use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum Operation {
    #[cfg(feature = "rfc9173")]
    AES_GCM(rfc9173::bcb_aes_gcm::Operation),
    Unrecognised(u64, parse::UnknownOperation),
}

pub struct OperationArgs<'a> {
    pub bpsec_source: &'a eid::Eid,
    pub target: &'a block::Block,
    pub target_number: u64,
    pub target_payload: &'a [u8],
    pub source: &'a block::Block,
    pub source_number: u64,
    pub primary_block: &'a [u8],
}

pub struct OperationResult {
    pub plaintext: Option<Zeroizing<Box<[u8]>>>,
    pub protects_primary_block: bool,
    pub can_encrypt: bool,
}

impl Operation {
    pub fn context_id(&self) -> Context {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(_) => Context::BCB_RFC9173_AES_GCM,
            Self::Unrecognised(id, _) => Context::Unrecognised(*id),
        }
    }

    pub fn is_unsupported(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Operation::AES_GCM(operation) => operation.is_unsupported(),
            Operation::Unrecognised(..) => true,
        }
    }

    pub fn encrypt(
        &mut self,
        key: Option<&KeyMaterial>,
        args: OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<Box<[u8]>, Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(op) => op.encrypt(key, args, payload_data),
            Self::Unrecognised(v, _) => Err(Error::UnrecognisedContext(*v)),
        }
    }

    pub fn decrypt(
        &self,
        key: Option<&KeyMaterial>,
        args: OperationArgs,
        payload_data: Option<&[u8]>,
    ) -> Result<OperationResult, Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(op) => op.decrypt(key, args, payload_data),
            Self::Unrecognised(..) => Ok(OperationResult {
                plaintext: None,
                protects_primary_block: args.target_number == 0,
                can_encrypt: false,
            }),
        }
    }

    fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(self, array: &mut hardy_cbor::encode::Array) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::AES_GCM(o) => o.emit_result(array),
            Self::Unrecognised(_, o) => o.emit_result(array),
        }
    }
}

pub struct OperationSet {
    pub source: eid::Eid,
    pub operations: HashMap<u64, Operation>,
}

impl OperationSet {
    pub fn is_unsupported(&self) -> bool {
        self.operations.values().next().unwrap().is_unsupported()
    }
}

impl hardy_cbor::encode::ToCbor for OperationSet {
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

impl hardy_cbor::decode::FromCbor for OperationSet {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        let Some((asb, shortest, len)) =
            hardy_cbor::decode::try_parse::<(parse::AbstractSyntaxBlock, bool, usize)>(data)?
        else {
            return Ok(None);
        };

        // Unpack into strong types
        match asb.context {
            #[cfg(feature = "rfc9173")]
            Context::BCB_RFC9173_AES_GCM => {
                rfc9173::bcb_aes_gcm::parse(asb, data).map(|(source, operations, s)| {
                    Some((OperationSet { source, operations }, shortest && s, len))
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
