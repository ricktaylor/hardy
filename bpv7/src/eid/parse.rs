use super::*;
use error::CaptureFieldErr;
use std::borrow::Cow;
use winnow::{
    ModalResult, Parser,
    ascii::dec_uint,
    combinator::{alt, fail, opt, preceded, repeat, separated, terminated},
    stream::AsChar,
    token::{one_of, take_while},
};

fn parse_ipn_parts(input: &mut &[u8]) -> ModalResult<Eid> {
    (
        dec_uint,
        preceded(".", dec_uint),
        opt(preceded(".", dec_uint)),
    )
        .try_map(|(a, b, c)| match (a, b, c) {
            (0, 0, Some(0)) | (0, 0, None) => Ok(Eid::Null),
            (0, 0, Some(service_number)) | (0, service_number, None) => {
                Err(EidError::IpnInvalidServiceNumber(service_number as u64))
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

fn parse_local_node(input: &mut &[u8]) -> ModalResult<Eid> {
    preceded("!.", dec_uint)
        .map(|service_number| Eid::LocalNode { service_number })
        .parse_next(input)
}

fn parse_ipn(input: &mut &[u8]) -> ModalResult<Eid> {
    (alt((parse_local_node, parse_ipn_parts, fail))).parse_next(input)
}

fn from_hex_digit(digit: u8) -> u8 {
    match digit {
        b'0'..=b'9' => digit - b'0',
        b'A'..=b'F' => digit - b'A' + 10,
        _ => digit - b'a' + 10,
    }
}

fn parse_pchar<'a>(input: &mut &'a [u8]) -> ModalResult<Cow<'a, str>> {
    alt((
        take_while(
            1..,
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
                ':',
                '@',
            ),
        )
        .map(|v| Cow::Borrowed(unsafe { std::str::from_utf8_unchecked(v) })),
        preceded(
            '%',
            (one_of(AsChar::is_hex_digit), one_of(AsChar::is_hex_digit)),
        )
        .map(|(first, second)| {
            /* This is a more cautious UTF8 url decode expansion */
            let first = from_hex_digit(first);
            let second = from_hex_digit(second);
            if first <= 7 {
                let val = [(first << 4) | second];
                Cow::Owned(unsafe { std::str::from_utf8_unchecked(&val) }.into())
            } else {
                let val = [0xC0u8 | (first >> 2), 0x80u8 | ((first & 3) << 4) | second];
                Cow::Owned(unsafe { std::str::from_utf8_unchecked(&val) }.into())
            }
        }),
        fail,
    ))
    .parse_next(input)
}

fn parse_dtn_parts(input: &mut &[u8]) -> ModalResult<Eid> {
    (
        terminated(
            repeat(1.., parse_pchar)
                .fold(String::new, |mut acc, v| {
                    if acc.is_empty() {
                        acc = v.into()
                    } else {
                        acc.push_str(&v)
                    }
                    acc
                })
                .map(Into::<Box<str>>::into),
            "/",
        ),
        separated(
            0..,
            repeat(0.., parse_pchar)
                .fold(String::new, |mut acc, v| {
                    if acc.is_empty() {
                        acc = v.into()
                    } else {
                        acc.push_str(&v)
                    }
                    acc
                })
                .map(Into::<Box<str>>::into),
            "/",
        ),
    )
        .map(|(node_name, demux): (Box<str>, Vec<Box<str>>)| Eid::Dtn {
            node_name,
            demux: demux.into(),
        })
        .parse_next(input)
}

fn parse_dtn(input: &mut &[u8]) -> ModalResult<Eid> {
    alt((
        "none".map(|_| Eid::Null),
        preceded("//", parse_dtn_parts),
        fail,
    ))
    .parse_next(input)
}

pub fn parse_eid(input: &mut &[u8]) -> ModalResult<Eid> {
    alt((
        preceded("dtn:", parse_dtn),
        preceded("ipn:", parse_ipn),
        fail,
    ))
    .parse_next(input)
}

impl std::str::FromStr for Eid {
    type Err = EidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_eid
            .parse(s.as_bytes())
            .map_err(|e| EidError::ParseError(e.to_string()))
    }
}

impl TryFrom<&str> for Eid {
    type Error = EidError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<Eid> for String {
    fn from(value: Eid) -> Self {
        value.to_string()
    }
}

fn ipn_from_cbor(value: &mut cbor::decode::Array, shortest: bool) -> Result<(Eid, bool), EidError> {
    let (a, s1) = value.parse()?;
    let (b, s2) = value.parse()?;
    let (c, shortest) = if let Some((c, s3)) = value.try_parse()? {
        (Some(c), shortest && s1 && s2 && s3)
    } else {
        (None, shortest && s1 && s2)
    };

    const MAX: u64 = u32::MAX as u64;

    match (a, b, c) {
        (0, 0, Some(0)) => Ok((Eid::Null, false)),
        (0, 0, None) => Ok((Eid::Null, shortest)),
        (0, 0, Some(s)) | (0, s, None) => Err(EidError::IpnInvalidServiceNumber(s)),
        (a, _, Some(_)) if a > MAX => Err(EidError::IpnInvalidAllocatorId(a)),
        (_, n, Some(_)) if n > MAX => Err(EidError::IpnInvalidNodeNumber(n)),
        (_, _, Some(s)) | (_, s, None) if s > MAX => Err(EidError::IpnInvalidServiceNumber(s)),
        (0, MAX, Some(s)) | (MAX, s, None) => Ok((
            Eid::LocalNode {
                service_number: s as u32,
            },
            false,
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
        (n, s, None) if n <= MAX => Ok((
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
                node_number: (fqnn & MAX) as u32,
                service_number: s as u32,
            },
            shortest,
        )),
    }
}

impl cbor::decode::FromCbor for Eid {
    type Error = error::EidError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, mut shortest, tags| {
            shortest = shortest && tags.is_empty() && a.is_definite();

            match a
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("EID scheme")?
            {
                0 => Err(EidError::UnsupportedScheme(0)),
                1 => a
                    .parse_value(|value, s, tags| {
                        shortest = shortest && s && tags.is_empty();
                        match value {
                            cbor::decode::Value::UnsignedInteger(0)
                            | cbor::decode::Value::Text("none") => Ok((Eid::Null, shortest)),
                            cbor::decode::Value::Text(s) => parse_dtn
                                .parse(s.as_bytes())
                                .map(|e| (e, shortest))
                                .map_err(|e| EidError::ParseError(e.to_string())),
                            cbor::decode::Value::TextStream(s) => {
                                let s = s.iter().fold(String::new(), |mut acc, s| {
                                    acc.push_str(s);
                                    acc
                                });
                                parse_dtn
                                    .parse(s.as_bytes())
                                    .map(|e| (e, shortest))
                                    .map_err(|e| EidError::ParseError(e.to_string()))
                            }
                            value => Err(cbor::decode::Error::IncorrectType(
                                "Untagged Text String or O".to_string(),
                                value.type_name(!tags.is_empty()),
                            )
                            .into()),
                        }
                    })
                    .map_field_err("'dtn' scheme-specific part"),
                2 => match a.parse_value(|value, s, tags| match value {
                    cbor::decode::Value::Array(a) => {
                        ipn_from_cbor(a, shortest && s && tags.is_empty() && a.is_definite())
                    }
                    value => Err(cbor::decode::Error::IncorrectType(
                        "Untagged Array".to_string(),
                        value.type_name(!tags.is_empty()),
                    )
                    .into()),
                }) {
                    Err(EidError::InvalidCBOR(e)) => {
                        Err(e).map_field_err("'ipn' scheme-specific part")
                    }
                    r => r,
                },
                scheme => {
                    let start = a.offset();
                    if a.skip_value(16).map_err(Into::<EidError>::into)?.is_none() {
                        Err(EidError::UnsupportedScheme(scheme))
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
