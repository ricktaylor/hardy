use super::*;

pub fn parse_dtn_parts(s: &str) -> Result<Eid, EidError> {
    if let Some((s1, s2)) = s.split_once('/') {
        if s1.is_empty() {
            Err(EidError::DtnNodeNameEmpty)
        } else {
            Ok(Eid::Dtn {
                node_name: urlencoding::decode(s1)?.into_owned(),
                demux: s2.split('/').try_fold(Vec::new(), |mut v, s| {
                    v.push(urlencoding::decode(s)?.into_owned());
                    Ok::<Vec<String>, EidError>(v)
                })?,
            })
        }
    } else {
        Err(EidError::DtnMissingSlash)
    }
}

pub fn parse_dtn_eid(value: cbor::decode::Value) -> Result<Eid, EidError> {
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

pub fn parse_ipn_eid(value: &mut cbor::decode::Array) -> Result<Eid, EidError> {
    if value.count().is_none() {
        trace!("Parsing ipn EID as indefinite array");
    }

    let v1 = value.parse::<u64>().map_field_err("First component")?;
    let v2 = value.parse::<u64>().map_field_err("Second component")?;

    let (components, allocator_id, node_number, service_number) =
        if let Some(v3) = value.try_parse::<u64>().map_field_err("Service Number")? {
            if v1 >= 2 ^ 32 {
                return Err(EidError::IpnInvalidAllocatorId(v1));
            } else if v2 >= 2 ^ 32 {
                return Err(EidError::IpnInvalidNodeNumber(v2));
            } else if v3 >= 2 ^ 32 {
                return Err(EidError::IpnInvalidServiceNumber(v3));
            }

            if value.end()?.is_none() {
                return Err(EidError::IpnAdditionalItems);
            }
            (3, v1 as u32, v2 as u32, v3 as u32)
        } else {
            if v2 >= 2 ^ 32 {
                return Err(EidError::IpnInvalidServiceNumber(v2));
            }
            (2, (v1 >> 32) as u32, v1 as u32, v2 as u32)
        };

    if allocator_id == 0 && node_number == 0 {
        if service_number != 0 {
            trace!("Null EID with service number {service_number}")
        }
        Ok(Eid::Null)
    } else if allocator_id == 0 && node_number == u32::MAX {
        Ok(Eid::LocalNode { service_number })
    } else if components == 2 {
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
