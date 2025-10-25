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

    pub fn verify(&self, key_f: &impl key::KeyStore, args: OperationArgs) -> Result<(), Error> {
        match self {
            #[cfg(feature = "rfc9173")]
            Self::HMAC_SHA2(o) => o.verify(key_f, args),
            Self::Unrecognised(..) => Err(Error::NoValidKey),
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
    type Result = ();

    fn to_cbor(&self, encoder: &mut hardy_cbor::encode::Encoder) -> Self::Result {
        // Ensure we process operations in the same order
        let (targets, operations): (Vec<&u64>, Vec<&Operation>) = self.operations.iter().unzip();

        // Targets
        encoder.emit(targets.as_slice());

        // Context
        operations
            .first()
            .unwrap()
            .emit_context(encoder, &self.source);

        // Results
        encoder.emit_array(Some(operations.len()), |a| {
            for op in operations {
                op.emit_result(a);
            }
        });
    }
}

impl hardy_cbor::decode::FromCbor for OperationSet {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        let (asb, shortest, len) =
            hardy_cbor::decode::parse::<(parse::AbstractSyntaxBlock, bool, usize)>(data)?;

        // Unpack into strong types
        match asb.context {
            #[cfg(feature = "rfc9173")]
            Context::BIB_HMAC_SHA2 => {
                rfc9173::bib_hmac_sha2::parse(asb, data).map(|(source, operations, s)| {
                    (OperationSet { source, operations }, shortest && s, len)
                })
            }
            Context::Unrecognised(id) => {
                parse::UnknownOperation::parse(asb, data).map(|(source, operations)| {
                    (
                        OperationSet {
                            source,
                            operations: operations
                                .into_iter()
                                .map(|(t, o)| (t, Operation::Unrecognised(id, o)))
                                .collect(),
                        },
                        shortest,
                        len,
                    )
                })
            }
            c => Err(Error::InvalidContext(c)),
        }
    }
}
