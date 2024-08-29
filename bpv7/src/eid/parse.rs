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
) -> Result<Eid, EidError> {
    if allocator_id == 0 && node_number == 0 {
        if service_number != 0 {
            trace!("Null EID with service number {service_number}")
        }
        Ok(Eid::Null)
    } else if allocator_id == 0 && node_number == u32::MAX {
        Ok(Eid::LocalNode { service_number })
    } else if elements == 2 {
        Ok(Eid::Ipn2 {
            allocator_id,
            node_number,
            service_number,
        })
    } else {
        Ok(Eid::Ipn3 {
            allocator_id,
            node_number,
            service_number,
        })
    }
}

fn ipn_from_str(s: &str) -> Result<Eid, EidError> {
    let parts = s.split('.').collect::<Vec<&str>>();
    if parts.len() == 2 {
        let mut node_number = u32::MAX;
        if parts[0] != "!" {
            node_number = parts[0].parse::<u32>().map_field_err("Node Number")?;
        }
        ipn_from_parts(
            3,
            0,
            node_number,
            parts[1].parse::<u32>().map_field_err("Service Number")?,
        )
    } else if parts.len() == 3 {
        ipn_from_parts(
            3,
            parts[0]
                .parse::<u32>()
                .map_field_err("Allocator Identifier")?,
            parts[1].parse::<u32>().map_field_err("Node Number")?,
            parts[2].parse::<u32>().map_field_err("Service Number")?,
        )
    } else {
        return Err(EidError::IpnInvalidComponents);
    }
}

pub fn eid_from_str(s: &str) -> Result<Eid, EidError> {
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

fn ipn_from_cbor(value: &mut cbor::decode::Array) -> Result<Eid, EidError> {
    if !value.is_definite() {
        trace!("Parsing ipn EID as indefinite array");
    }

    let Some(v1) = value.try_parse::<u64>().map_field_err("First component")? else {
        return Err(EidError::IpnInvalidComponents);
    };
    let Some(v2) = value.try_parse::<u64>().map_field_err("Second component")? else {
        return Err(EidError::IpnInvalidComponents);
    };

    if let Some(v3) = value.try_parse::<u64>().map_field_err("Service Number")? {
        if v1 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidAllocatorId(v1));
        } else if v2 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidNodeNumber(v2));
        } else if v3 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidServiceNumber(v3));
        }

        if value.end()?.is_none() {
            return Err(EidError::IpnInvalidComponents);
        }
        ipn_from_parts(3, v1 as u32, v2 as u32, v3 as u32)
    } else {
        if v2 > u32::MAX as u64 {
            return Err(EidError::IpnInvalidServiceNumber(v2));
        }
        ipn_from_parts(2, (v1 >> 32) as u32, v1 as u32, v2 as u32)
    }
}

pub fn eid_from_cbor(data: &[u8]) -> Result<Option<(Eid, usize)>, EidError> {
    cbor::decode::try_parse_array(data, |a, tags| {
        if !tags.is_empty() {
            return Err(cbor::decode::Error::IncorrectType(
                "Untagged Array".to_string(),
                "Tagged Array".to_string(),
            )
            .into());
        }

        if !a.is_definite() {
            trace!("Parsing EID array of indefinite length")
        }

        match a.parse::<u64>().map_field_err("Scheme")? {
            0 => Err(EidError::UnsupportedScheme("0".to_string())),
            1 => match a
                .parse_value(|value, _, tags| match (value, !tags.is_empty()) {
                    (cbor::decode::Value::UnsignedInteger(0), false)
                    | (cbor::decode::Value::Text("none", _), false) => Ok(Eid::Null),
                    (cbor::decode::Value::Text(s, _), false) => {
                        if let Some(s) = s.strip_prefix("//") {
                            parse_dtn_parts(s)
                        } else {
                            Err(EidError::DtnMissingPrefix)
                        }
                    }
                    (value, tagged) => Err(cbor::decode::Error::IncorrectType(
                        "Untagged Text String or O".to_string(),
                        value.type_name(tagged),
                    )
                    .into()),
                })
                .map(|(eid, _)| eid)
            {
                Err(EidError::InvalidCBOR(e)) => Err(e).map_field_err("'dtn' scheme-specific part"),
                Err(EidError::InvalidUtf8(e)) => Err(e).map_field_err("'dtn' scheme-specific part"),
                r => r,
            },
            2 => match a
                .parse_value(|value, _, tags| match (value, !tags.is_empty()) {
                    (cbor::decode::Value::Array(a), false) => ipn_from_cbor(a),
                    (value, tagged) => Err(cbor::decode::Error::IncorrectType(
                        "Untagged Array".to_string(),
                        value.type_name(tagged),
                    )
                    .into()),
                })
                .map(|(eid, _)| eid)
            {
                Err(EidError::InvalidCBOR(e)) => Err(e).map_field_err("'ipn' scheme-specific part"),
                Err(EidError::InvalidUtf8(e)) => Err(e).map_field_err("'ipn' scheme-specific part"),
                r => r,
            },
            scheme => {
                if let Some((start, len)) = a.skip_value(16).map_err(Into::<EidError>::into)? {
                    Ok(Eid::Unknown {
                        scheme,
                        data: data[start..start + len].into(),
                    })
                } else {
                    Err(EidError::UnsupportedScheme(scheme.to_string()))
                }
            }
        }
    })
}
