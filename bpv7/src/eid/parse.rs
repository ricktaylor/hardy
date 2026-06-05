use super::*;
use crate::error::{CaptureFieldErr, HasInvalidField};
use percent_encoding::percent_decode_str;
use winnow::{
    ModalResult, Parser,
    ascii::dec_uint,
    combinator::{alt, opt, preceded, terminated},
    stream::AsChar,
    token::take_while,
};

fn parse_ipn_parts(input: &mut &str) -> ModalResult<Eid> {
    (
        dec_uint,
        preceded(".", dec_uint),
        opt(preceded(".", dec_uint)),
    )
        .try_map(|(a, b, c)| match (a, b, c) {
            (0, 0, Some(0)) | (0, 0, None) => Ok(Eid::Null),
            (0, 0, Some(service_number)) | (0, service_number, None) => {
                Err(Error::IpnInvalidServiceNumber(service_number as u64))
            }
            (0, u32::MAX, Some(service_number)) | (u32::MAX, service_number, None) => {
                Ok(Eid::LocalNode(service_number))
            }
            (allocator_id, node_number, Some(service_number)) => Ok(Eid::Ipn {
                fqnn: IpnNodeId {
                    allocator_id,
                    node_number,
                },
                service_number,
            }),
            (node_number, service_number, None) => Ok(Eid::Ipn {
                fqnn: IpnNodeId {
                    allocator_id: 0,
                    node_number,
                },
                service_number,
            }),
        })
        .parse_next(input)
}

fn parse_local_node(input: &mut &str) -> ModalResult<Eid> {
    preceded("!.", dec_uint)
        .map(Eid::LocalNode)
        .parse_next(input)
}

fn parse_ipn(input: &mut &str) -> ModalResult<Eid> {
    (alt((parse_local_node, parse_ipn_parts))).parse_next(input)
}

fn parse_regname(input: &mut &str) -> ModalResult<DtnNodeId> {
    take_while(0.., |c: char| {
        c.is_alphanum()
            || matches!(
                c,
                '-' | '.'
                    | '_'
                    | '~'
                    | '!'
                    | '$'
                    | '&'
                    | '\''
                    | '('
                    | ')'
                    | '*'
                    | '+'
                    | ','
                    | ';'
                    | '='
                    | '%'
            )
    })
    .try_map(|v| {
        percent_decode_str(v).decode_utf8().map(|s| DtnNodeId {
            node_name: s.into_owned().into(),
        })
    })
    .parse_next(input)
}

fn parse_dtn_service_name(input: &mut &str) -> ModalResult<Box<str>> {
    take_while(0.., '\x21'..='\x7e')
        .map(Box::from)
        .parse_next(input)
}

impl DtnNodeId {
    pub fn is_valid_service_name(mut service_name: &str) -> bool {
        parse_dtn_service_name(&mut service_name).is_ok()
    }
}

fn parse_dtn_parts(input: &mut &str) -> ModalResult<Eid> {
    (terminated(parse_regname, "/"), parse_dtn_service_name)
        .map(|(node_name, service_name)| Eid::Dtn {
            node_name,
            service_name,
        })
        .parse_next(input)
}

fn parse_dtn(input: &mut &str) -> ModalResult<Eid> {
    alt(("none".map(|_| Eid::Null), preceded("//", parse_dtn_parts))).parse_next(input)
}

pub fn parse_eid(input: &mut &str) -> ModalResult<Eid> {
    alt((preceded("dtn:", parse_dtn), preceded("ipn:", parse_ipn))).parse_next(input)
}

impl core::str::FromStr for NodeId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Eid::from_str(s)?.try_into()
    }
}

impl core::str::FromStr for Eid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_eid
            .parse(s)
            .map_err(|e| Error::ParseError(e.to_string()))
    }
}

impl TryFrom<&str> for NodeId {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<&str> for Eid {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<Cow<'_, str>> for NodeId {
    type Error = Error;

    fn try_from(value: Cow<'_, str>) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl TryFrom<Cow<'_, str>> for Eid {
    type Error = Error;

    fn try_from(value: Cow<'_, str>) -> Result<Self, Self::Error> {
        value.parse()
    }
}

/// Decodes an `ipn` SSP array (2-element legacy or 3-element per RFC 9758).
///
/// Non-shortest individual uints are rejected as `NotCanonical` (RFC 9171
/// §4.1: scalars MUST be deterministic; uints have no indefinite variant).
/// `canonical` is the canonical-shortest signal for the wrapping
/// array passed by the caller (false if the outer EID array or this SSP
/// array used indefinite-length encoding); the returned bool combines
/// that with the per-encoding canonical signal (3-element form with
/// `allocator_id=0` is RFC-valid but not the recommended form per
/// RFC 9758 §6.1.2, so returns `false` to trigger a rewrite).
fn ipn_from_cbor(
    value: &mut hardy_cbor::decode::Array,
    canonical: bool,
) -> Result<(Eid, bool), Error> {
    let (a, s1): (u64, bool) = value.parse()?;
    if !s1 {
        return Err(Error::NotCanonical);
    }
    let (b, s2): (u64, bool) = value.parse()?;
    if !s2 {
        return Err(Error::NotCanonical);
    }
    let c: Option<u64> = if let Some((c, s3, _)) = value.try_parse::<(u64, bool, usize)>()? {
        if !s3 {
            return Err(Error::NotCanonical);
        }
        Some(c)
    } else {
        None
    };

    const U32_MAX: u64 = u32::MAX as u64;

    match (a, b, c) {
        (0, 0, Some(0)) | (0, 0, None) => Ok((Eid::Null, canonical)),
        (0, 0, Some(_)) | (0, _, None) => Ok((Eid::Null, false)),
        (a, _, Some(_)) if a > U32_MAX => Err(Error::IpnInvalidAllocatorId(a)),
        (_, n, Some(_)) if n > U32_MAX => Err(Error::IpnInvalidNodeNumber(n)),
        (_, _, Some(s)) | (_, s, None) if s > U32_MAX => Err(Error::IpnInvalidServiceNumber(s)),
        (0, U32_MAX, Some(s)) | (U32_MAX, s, None) => Ok((Eid::LocalNode(s as u32), canonical)),
        // 3-element form with allocator=0: RFC 9758 §6.1.2 RECOMMENDS
        // the 2-element form here, so flag for rewrite.
        (0, n, Some(s)) => Ok((
            Eid::Ipn {
                fqnn: IpnNodeId {
                    allocator_id: 0,
                    node_number: n as u32,
                },
                service_number: s as u32,
            },
            false,
        )),
        (a, n, Some(s)) => Ok((
            Eid::Ipn {
                fqnn: IpnNodeId {
                    allocator_id: a as u32,
                    node_number: n as u32,
                },
                service_number: s as u32,
            },
            canonical,
        )),
        (n, s, None) if n <= U32_MAX => Ok((
            Eid::Ipn {
                fqnn: IpnNodeId {
                    allocator_id: 0,
                    node_number: n as u32,
                },
                service_number: s as u32,
            },
            canonical,
        )),
        (fqnn, s, None) => Ok((
            Eid::LegacyIpn {
                fqnn: IpnNodeId {
                    allocator_id: (fqnn >> 32) as u32,
                    node_number: (fqnn & U32_MAX) as u32,
                },
                service_number: s as u32,
            },
            canonical,
        )),
    }
}

impl hardy_cbor::decode::FromCbor for Eid {
    type Error = error::Error;

    /// Strict-canonical decode with the RFC 9171 §4.1 indefinite-length
    /// carveout.
    ///
    /// Rejected as `NotCanonical`:
    ///   * non-shortest outer array head, non-shortest scheme uint,
    ///     non-shortest SSP scalars (uints, text head)
    ///   * unexpected tags on any item
    ///
    /// Accepted but flagged `shortest == false` (caller may re-emit
    /// canonical):
    ///   * indefinite-length outer EID array
    ///   * indefinite-length `ipn` SSP array
    ///   * indefinite-length dtn text (CBOR text stream)
    ///   * dtn null encoded as `Text("none")` (RFC 9171 §4.2.5.1.1
    ///     mandates `uint 0`; the text form is unambiguous on the wire
    ///     because any real dtn URI's SSP starts with "//", so we
    ///     accept and queue a rewrite to uint 0)
    ///   * 3-element ipn EID with `allocator_id == 0` (RFC 9758
    ///     §6.1.2 recommends the 2-element form)
    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |a, s, tags| {
            if !s || !tags.is_empty() {
                return Err(Error::NotCanonical);
            }
            // Indefinite-length outer EID array: RFC-permitted but not
            // canonical-shortest. The carry-through bool below picks
            // this up so the returned `shortest` flag reflects it.
            let canonical = a.is_definite();

            let (scheme, s): (u64, bool) = a.parse().map_field_err::<Error>("EID scheme")?;
            if !s {
                return Err(Error::invalid_field(
                    "EID scheme",
                    Error::NotCanonical.into(),
                ));
            }

            match scheme {
                0 => Err(Error::UnsupportedScheme(0)),
                1 => a
                    .parse_value(|value, s, tags| {
                        if !tags.is_empty() {
                            return Err(Error::NotCanonical);
                        }
                        match value {
                            hardy_cbor::decode::Value::UnsignedInteger(0) => {
                                if !s {
                                    return Err(Error::NotCanonical);
                                }
                                Ok((Eid::Null, canonical))
                            }
                            hardy_cbor::decode::Value::Text("none") => {
                                if !s {
                                    return Err(Error::NotCanonical);
                                }
                                // Non-canonical: §4.2.5.1.1 says uint 0.
                                Ok((Eid::Null, false))
                            }
                            hardy_cbor::decode::Value::Text(text) => {
                                if !s {
                                    return Err(Error::NotCanonical);
                                }
                                parse_dtn
                                    .parse(text)
                                    .map(|e| (e, canonical))
                                    .map_err(|e| Error::ParseError(e.to_string()))
                            }
                            hardy_cbor::decode::Value::TextStream(parts) => {
                                // Indefinite-length text: RFC-permitted,
                                // non-canonical.
                                let combined = parts.iter().fold(String::new(), |mut acc, s| {
                                    acc.push_str(s);
                                    acc
                                });
                                parse_dtn
                                    .parse(&combined)
                                    .map(|e| (e, false))
                                    .map_err(|e| Error::ParseError(e.to_string()))
                            }
                            value => Err(hardy_cbor::decode::Error::IncorrectType(
                                "Untagged Text String or Unsigned Integer 0".to_string(),
                                value.type_name(false),
                            )
                            .into()),
                        }
                    })
                    .map_field_err::<Error>("'dtn' scheme-specific part"),
                2 => match a.parse_value(|value, s, tags| match value {
                    hardy_cbor::decode::Value::Array(arr) => {
                        if !s || !tags.is_empty() {
                            return Err(Error::NotCanonical);
                        }
                        ipn_from_cbor(arr, canonical && arr.is_definite())
                    }
                    value => Err(hardy_cbor::decode::Error::IncorrectType(
                        "Untagged Array".to_string(),
                        value.type_name(!tags.is_empty()),
                    )
                    .into()),
                }) {
                    Err(Error::InvalidCBOR(e)) => {
                        Err(e).map_field_err::<Error>("'ipn' scheme-specific part")
                    }
                    r => r,
                },
                scheme => {
                    // Unknown scheme: we can't validate the content;
                    // stash the raw bytes for round-trip. Canonical
                    // flag reflects only the outer-array shape.
                    let start = a.offset();
                    a.skip_value(16)?;
                    Ok((
                        Eid::Unknown {
                            scheme,
                            data: data[start..a.offset()].into(),
                        },
                        canonical,
                    ))
                }
            }
        })
        .map(|((v, s), len)| (v, s, len))
    }
}
