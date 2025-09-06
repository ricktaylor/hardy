use super::*;
use error::CaptureFieldErr;
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
                Ok(Eid::LocalNode { service_number })
            }
            (allocator_id, node_number, Some(service_number)) => Ok(Eid::Ipn {
                allocator_id,
                node_number,
                service_number,
            }),
            (node_number, service_number, None) => Ok(Eid::Ipn {
                allocator_id: 0,
                node_number,
                service_number,
            }),
        })
        .parse_next(input)
}

fn parse_local_node(input: &mut &str) -> ModalResult<Eid> {
    preceded("!.", dec_uint)
        .map(|service_number| Eid::LocalNode { service_number })
        .parse_next(input)
}

fn parse_ipn(input: &mut &str) -> ModalResult<Eid> {
    (alt((parse_local_node, parse_ipn_parts))).parse_next(input)
}

fn parse_regname(input: &mut &str) -> ModalResult<Box<str>> {
    take_while(
        0..,
        (
            AsChar::is_alphanum,
            '-',
            '.',
            '_',
            '~',
            '!',
            '$',
            '&',
            '\'',
            '(',
            ')',
            '*',
            '+',
            ',',
            ';',
            '=',
            ('%', AsChar::is_hex_digit, AsChar::is_hex_digit),
        ),
    )
    .try_map(|v| urlencoding::decode(v).map(|s| s.into_owned().into()))
    .parse_next(input)
}

fn parse_dtn_parts(input: &mut &str) -> ModalResult<Eid> {
    (
        terminated(parse_regname, "/"),
        take_while(0.., '\x21'..='\x7e'),
    )
        .map(|(node_name, demux)| Eid::Dtn {
            node_name,
            demux: demux.into(),
        })
        .parse_next(input)
}

fn parse_dtn(input: &mut &str) -> ModalResult<Eid> {
    alt(("none".map(|_| Eid::Null), preceded("//", parse_dtn_parts))).parse_next(input)
}

pub fn parse_eid(input: &mut &str) -> ModalResult<Eid> {
    alt((preceded("dtn:", parse_dtn), preceded("ipn:", parse_ipn))).parse_next(input)
}

impl core::str::FromStr for Eid {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_eid
            .parse(s)
            .map_err(|e| Error::ParseError(e.to_string()))
    }
}

impl TryFrom<&str> for Eid {
    type Error = Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Eid> for String {
    fn from(value: Eid) -> Self {
        value.to_string()
    }
}

fn ipn_from_cbor(
    value: &mut hardy_cbor::decode::Array,
    shortest: bool,
) -> Result<(Eid, bool), Error> {
    let (a, s1) = value.parse()?;
    let (b, s2) = value.parse()?;
    let (c, shortest) = if let Some((c, s3)) = value.try_parse()? {
        (Some(c), shortest && s1 && s2 && s3)
    } else {
        (None, shortest && s1 && s2)
    };

    const U32_MAX: u64 = u32::MAX as u64;

    match (a, b, c) {
        (0, 0, Some(0)) => Ok((Eid::Null, false)),
        (0, 0, None) => Ok((Eid::Null, shortest)),
        (0, 0, Some(s)) | (0, s, None) => Err(Error::IpnInvalidServiceNumber(s)),
        (a, _, Some(_)) if a > U32_MAX => Err(Error::IpnInvalidAllocatorId(a)),
        (_, n, Some(_)) if n > U32_MAX => Err(Error::IpnInvalidNodeNumber(n)),
        (_, _, Some(s)) | (_, s, None) if s > U32_MAX => Err(Error::IpnInvalidServiceNumber(s)),
        (0, U32_MAX, Some(s)) | (U32_MAX, s, None) => Ok((
            Eid::LocalNode {
                service_number: s as u32,
            },
            shortest,
        )),
        (0, n, Some(s)) => Ok((
            Eid::Ipn {
                allocator_id: 0,
                node_number: n as u32,
                service_number: s as u32,
            },
            false,
        )),
        (a, n, Some(s)) => Ok((
            Eid::Ipn {
                allocator_id: a as u32,
                node_number: n as u32,
                service_number: s as u32,
            },
            shortest,
        )),
        (n, s, None) if n <= U32_MAX => Ok((
            Eid::Ipn {
                allocator_id: 0,
                node_number: n as u32,
                service_number: s as u32,
            },
            shortest,
        )),
        (fqnn, s, None) => Ok((
            Eid::LegacyIpn {
                allocator_id: (fqnn >> 32) as u32,
                node_number: (fqnn & U32_MAX) as u32,
                service_number: s as u32,
            },
            shortest,
        )),
    }
}

impl hardy_cbor::decode::TryFromCbor for Eid {
    type Error = error::Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && tags.is_empty() && a.is_definite();

            match a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("EID scheme")?
            {
                0 => Err(Error::UnsupportedScheme(0)),
                1 => a
                    .parse_value(|value, s, tags| {
                        shortest = shortest && s && tags.is_empty();
                        match value {
                            hardy_cbor::decode::Value::UnsignedInteger(0)
                            | hardy_cbor::decode::Value::Text("none") => Ok((Eid::Null, shortest)),
                            hardy_cbor::decode::Value::Text(s) => parse_dtn
                                .parse(s)
                                .map(|e| (e, shortest))
                                .map_err(|e| Error::ParseError(e.to_string())),
                            hardy_cbor::decode::Value::TextStream(s) => {
                                let s = s.iter().fold(String::new(), |mut acc, s| {
                                    acc.push_str(s);
                                    acc
                                });
                                parse_dtn
                                    .parse(&s)
                                    .map(|e| (e, shortest))
                                    .map_err(|e| Error::ParseError(e.to_string()))
                            }
                            value => Err(hardy_cbor::decode::Error::IncorrectType(
                                "Untagged Text String or O".to_string(),
                                value.type_name(!tags.is_empty()),
                            )
                            .into()),
                        }
                    })
                    .map_field_err("'dtn' scheme-specific part"),
                2 => match a.parse_value(|value, s, tags| match value {
                    hardy_cbor::decode::Value::Array(a) => {
                        ipn_from_cbor(a, shortest && s && tags.is_empty() && a.is_definite())
                    }
                    value => Err(hardy_cbor::decode::Error::IncorrectType(
                        "Untagged Array".to_string(),
                        value.type_name(!tags.is_empty()),
                    )
                    .into()),
                }) {
                    Err(Error::InvalidCBOR(e)) => {
                        Err(e).map_field_err("'ipn' scheme-specific part")
                    }
                    r => r,
                },
                scheme => {
                    let start = a.offset();
                    if a.skip_value(16).map_err(Into::<Error>::into)?.is_none() {
                        Err(Error::UnsupportedScheme(scheme))
                    } else {
                        Ok((
                            Eid::Unknown {
                                scheme,
                                data: data[start..a.offset()].into(),
                            },
                            shortest,
                        ))
                    }
                }
            }
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
