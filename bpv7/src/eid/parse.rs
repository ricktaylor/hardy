use super::*;

use error::CaptureFieldErr;

fn parse_dtn_parts(s: &str) -> Result<Eid, EidError> {
    if let Some((s1, s2)) = s.split_once('/') {
        if s1.is_empty() {
            Err(EidError::DtnNodeNameEmpty)
        } else {
            let node_name = urlencoding::decode(s1)?.into();
            let demux = s2
                .split('/')
                .try_fold(Vec::new(), |mut v: Vec<Box<str>>, s| {
                    v.push(urlencoding::decode(s)?.into());
                    Ok::<_, EidError>(v)
                })?;

            for (idx, s) in demux.iter().enumerate() {
                if s.is_empty() && idx != demux.len() - 1 {
                    return Err(EidError::DtnEmptyDemuxPart);
                }
            }

            Ok(Eid::Dtn {
                node_name,
                demux: demux.into(),
            })
        }
    } else {
        Err(EidError::DtnMissingSlash)
    }
}

fn ipn_from_parts(
    elements: usize,
    allocator_id: u32,
    node_number: u32,
    service_number: u32,
) -> Result<(Eid, bool), EidError> {
    if allocator_id == 0 && node_number == 0 {
        Ok((Eid::Null, service_number == 0))
    } else if allocator_id == 0 && node_number == u32::MAX {
        Ok((Eid::LocalNode { service_number }, true))
    } else if elements == 2 {
        Ok((
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            },
            true,
        ))
    } else {
        Ok((
            Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            },
            true,
        ))
    }
}

fn ipn_from_str(s: &str) -> Result<Eid, EidError> {
    let parts = s.split('.').collect::<Vec<&str>>();
    if parts.len() == 2 {
        let mut node_number = u32::MAX;
        if parts[0] != "!" {
            node_number = parts[0].parse().map_field_err("Node Number")?;
        }
        ipn_from_parts(
            3,
            0,
            node_number,
            parts[1].parse().map_field_err("Service Number")?,
        )
        .map(|(e, _)| e)
    } else if parts.len() == 3 {
        ipn_from_parts(
            3,
            parts[0].parse().map_field_err("Allocator Identifier")?,
            parts[1].parse().map_field_err("Node Number")?,
            parts[2].parse().map_field_err("Service Number")?,
        )
        .map(|(e, _)| e)
    } else {
        return Err(EidError::IpnInvalidComponents);
    }
}

impl std::str::FromStr for Eid {
    type Err = EidError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(s) = s.strip_prefix("dtn:") {
            if let Some(s) = s.strip_prefix("//") {
                parse_dtn_parts(s)
            } else if s == "none" {
                Ok(Eid::Null)
            } else {
                Err(EidError::DtnMissingPrefix)
            }
        } else if let Some(s) = s.strip_prefix("ipn:") {
            ipn_from_str(s)
        } else if let Some((schema, _)) = s.split_once(':') {
            Err(EidError::UnsupportedScheme(schema.to_string()))
        } else {
            Err(EidError::MissingScheme)
        }
    }
}

fn ipn_from_cbor(value: &mut cbor::decode::Array, shortest: bool) -> Result<(Eid, bool), EidError> {
    let (v1, s1) = value.parse()?;
    let (v2, s2) = value.parse()?;
    let v3 = value.try_parse()?;

    if let Some((v3, s3)) = v3 {
        if v1 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidAllocatorId(v1));
        } else if v2 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidNodeNumber(v2));
        } else if v3 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidServiceNumber(v3));
        }

        ipn_from_parts(3, v1 as u32, v2 as u32, v3 as u32)
            .map(|(e, s)| (e, shortest && s && s1 && s2 && s3))
    } else {
        if v2 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidServiceNumber(v2));
        }

        ipn_from_parts(2, (v1 >> 32) as u32, v1 as u32, v2 as u32)
            .map(|(e, s)| (e, shortest && s && s1 && s2))
    }
}

impl cbor::decode::FromCbor for Eid {
    type Error = error::EidError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |a, mut shortest, tags| {
            let (scheme, s) = a.parse().map_field_err("Scheme")?;
            shortest = shortest && tags.is_empty() && a.is_definite() && s;

            match scheme {
                0 => Err(EidError::UnsupportedScheme("0".to_string())),
                1 => match a
                    .parse_value(|value, s, tags| {
                        shortest = shortest && s && tags.is_empty();
                        match value {
                            cbor::decode::Value::UnsignedInteger(0) => Ok((Eid::Null, shortest)),
                            cbor::decode::Value::Text("none", chunked) => {
                                Ok((Eid::Null, shortest && !chunked))
                            }
                            cbor::decode::Value::Text(s, chunked) => {
                                if let Some(s) = s.strip_prefix("//") {
                                    parse_dtn_parts(s).map(|e| (e, shortest && !chunked))
                                } else {
                                    Err(EidError::DtnMissingPrefix)
                                }
                            }
                            value => Err(cbor::decode::Error::IncorrectType(
                                "Untagged Text String or O".to_string(),
                                value.type_name(!tags.is_empty()),
                            )
                            .into()),
                        }
                    })
                    .map(|((eid, shortest), _)| (eid, shortest))
                {
                    Err(EidError::InvalidCBOR(e)) => {
                        Err(e).map_field_err("'dtn' scheme-specific part")
                    }
                    Err(EidError::InvalidUtf8(e)) => {
                        Err(e).map_field_err("'dtn' scheme-specific part")
                    }
                    r => r,
                },
                2 => match a
                    .parse_value(|value, s, tags| match value {
                        cbor::decode::Value::Array(a) => {
                            ipn_from_cbor(a, shortest && s && tags.is_empty() && a.is_definite())
                        }
                        value => Err(cbor::decode::Error::IncorrectType(
                            "Untagged Array".to_string(),
                            value.type_name(!tags.is_empty()),
                        )
                        .into()),
                    })
                    .map(|((eid, shortest), _)| (eid, shortest))
                {
                    Err(EidError::InvalidCBOR(e)) => {
                        Err(e).map_field_err("'ipn' scheme-specific part")
                    }
                    Err(EidError::InvalidUtf8(e)) => {
                        Err(e).map_field_err("'ipn' scheme-specific part")
                    }
                    r => r,
                },
                scheme => {
                    let start = a.offset();
                    if let Some((_, len)) = a.skip_value(16).map_err(Into::<EidError>::into)? {
                        Ok((
                            Eid::Unknown {
                                scheme,
                                data: data[start..start + len].into(),
                            },
                            shortest,
                        ))
                    } else {
                        Err(EidError::UnsupportedScheme(scheme.to_string()))
                    }
                }
            }
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}
