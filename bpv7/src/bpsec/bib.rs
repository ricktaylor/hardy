use super::*;

#[allow(clippy::upper_case_acronyms)]
#[allow(non_camel_case_types)]
#[derive(Debug)]
pub enum Operation {
    #[cfg(feature = "rfc9173")]
    HMAC_SHA2(rfc9173::bib_hmac_sha2::Operation),
    Unrecognised(u64, parse::UnknownOperation),
}

pub struct OperationArgs<'a> {
    pub bpsec_source: &'a eid::Eid,
    pub target: u64,
    pub source: u64,
    pub blocks: &'a dyn BlockSet<'a>,
}

impl Operation {
    pub fn is_unsupported(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(operation) => operation.is_unsupported(),
            Self::Unrecognised(..) => true,
        }
    }

    pub fn protects_primary_block(&self) -> bool {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(operation) => operation.protects_primary_block(),
            Self::Unrecognised(..) => false,
        }
    }

    pub fn sign(jwk: &Key, args: OperationArgs) -> Result<Operation, Error> {
        #[cfg(feature = "rfc9173")]
        if let Some(op) = rfc9173::bib_hmac_sha2::Operation::sign(jwk, args)? {
            return Ok(Self::HMAC_SHA2(op));
        }

        Err(Error::InvalidKey(key::Operation::Sign, jwk.clone()))
    }

    pub fn verify_any(
        &self,
        key_f: &impl key::KeyStore,
        args: OperationArgs,
    ) -> Result<Option<bool>, Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.verify_any(key_f, args),
            Self::Unrecognised(..) => Ok(None),
        }
    }

    pub fn verify(&self, jwk: &Key, args: OperationArgs) -> Result<bool, Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.verify(jwk, args),
            Self::Unrecognised(v, _) => Err(Error::UnrecognisedContext(*v)),
        }
    }

    fn emit_context(&self, encoder: &mut hardy_cbor::encode::Encoder, source: &eid::Eid) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.emit_context(encoder, source),
            Self::Unrecognised(id, o) => o.emit_context(encoder, source, *id),
        }
    }

    fn emit_result(&self, array: &mut hardy_cbor::encode::Array) {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.emit_result(array),
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
    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) {
        // Ensure we process operations in the same order
        let ops = self.operations.iter().collect::<Vec<(&u64, &Operation)>>();

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

impl hardy_cbor::decode::TryFromCbor for OperationSet {
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
            Context::BIB_HMAC_SHA2 => {
                rfc9173::bib_hmac_sha2::parse(asb, data).map(|(source, operations, s)| {
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
