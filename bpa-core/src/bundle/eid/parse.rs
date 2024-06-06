use super::*;

use error::CaptureFieldErr;

fn parse_dtn_parts(s: &str) -> Result<Eid, EidError> {
    if let Some((s1, s2)) = s.split_once('/') {
        if s1.is_empty() {
            Err(EidError::DtnNodeNameEmpty)
        } else {
            let node_name = urlencoding::decode(s1)?.into_owned();
            let demux = s2.split('/').try_fold(Vec::new(), |mut v, s| {
                v.push(urlencoding::decode(s)?.into_owned());
                Ok::<Vec<String>, EidError>(v)
            })?;

            for (idx, s) in demux.iter().enumerate() {
                if s.is_empty() && idx != demux.len() - 1 {
                    return Err(EidError::DtnEmptyDemuxPart);
                }
            }

            Ok(Eid::Dtn { node_name, demux })
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

fn dtn_from_cbor(value: cbor::decode::Value) -> Result<Eid, EidError> {
    match value {
        cbor::decode::Value::UnsignedInteger(0) => Ok(Eid::Null),
        cbor::decode::Value::Text("none", _) => {
            trace!("Parsing dtn EID 'none'");
            Ok(Eid::Null)
        }
        cbor::decode::Value::Text(s, _) => {
            if let Some(s) = s.strip_prefix("//") {
                parse_dtn_parts(s)
            } else {
                Err(EidError::DtnMissingPrefix)
            }
        }
        _ => Err(EidError::DtnInvalidEncoding),
    }
}

fn ipn_from_cbor(value: &mut cbor::decode::Array) -> Result<Eid, EidError> {
    if value.count().is_none() {
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

pub fn eid_from_cbor(data: &[u8]) -> Result<(Eid, usize, Vec<u64>), EidError> {
    cbor::decode::parse_array(data, |a, tags| {
        if a.count().is_none() {
            trace!("Parsing EID array of indefinite length")
        }
        let schema = a.parse::<u64>().map_field_err("Scheme")?;
        let (eid, _) = a.parse_value(|value, _, tags2| {
            if !tags2.is_empty() {
                trace!("Parsing EID value with tags");
            }
            match (schema, value) {
                (1, value) => dtn_from_cbor(value),
                (2, cbor::decode::Value::Array(a)) => ipn_from_cbor(a),
                (2, value) => Err(cbor::decode::Error::IncorrectType(
                    "Array".to_string(),
                    value.type_name(),
                )
                .into()),
                _ => Err(EidError::UnsupportedScheme(schema.to_string())),
            }
        })?;
        if a.end()?.is_none() {
            Err(EidError::AdditionalItems)
        } else {
            Ok((eid, tags.to_vec()))
        }
    })
    .map(|((eid, tags), len)| (eid, len, tags))
}
