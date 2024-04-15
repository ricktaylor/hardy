use super::*;

#[derive(Default, Clone)]
pub enum Eid {
    #[default]
    Null,
    LocalNode {
        service_number: u32,
    },
    Ipn2 {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Ipn3 {
        allocator_id: u32,
        node_number: u32,
        service_number: u32,
    },
    Dtn {
        node_name: String,
        demux: String,
    },
}

impl Eid {
    fn parse_dtn_eid(value: cbor::decode::Value) -> Result<Eid, anyhow::Error> {
        match value {
            cbor::decode::Value::UnsignedInteger(0) => Ok(Self::Null),
            cbor::decode::Value::Text("none", _) => {
                log::info!("Parsing dtn EID 'none'");
                Ok(Self::Null)
            }
            cbor::decode::Value::Text(s, _) => {
                if !s.is_ascii() {
                    Err(anyhow!("dtn URI be ASCII"))
                } else if let Some(s) = s.strip_prefix("//") {
                    if let Some((s1, s2)) = s.split_once('/') {
                        if s1.is_empty() {
                            Err(anyhow!("dtn URI node-name is empty"))
                        } else {
                            Ok(Self::Dtn {
                                node_name: s1.to_string(),
                                demux: s2.to_string(),
                            })
                        }
                    } else {
                        Err(anyhow!("dtn URI missing name-delim '/'"))
                    }
                } else {
                    Err(anyhow!("dtn URI must start with '//'"))
                }
            }
            _ => Err(anyhow!("dtn URI is not a CBOR text string or 0")),
        }
    }

    fn parse_ipn_eid(mut value: cbor::decode::Array) -> Result<Eid, anyhow::Error> {
        if let Some(count) = value.count() {
            if !(2..=3).contains(&count) {
                return Err(anyhow!(
                    "IPN EIDs must be encoded as 2 or 3 element CBOR arrays"
                ));
            }
        } else {
            log::info!("Parsing IPN EID as indefinite array");
        }

        let v1 = value.parse::<u64>()?;
        let v2 = value.parse::<u64>()?;

        let (components, allocator_id, node_number, service_number) = if let Some(v3) =
            value.try_parse::<u64>()?
        {
            if (v1 >= 2 ^ 32) || (v2 >= 2 ^ 32) || (v3 >= 2 ^ 32) {
                return Err(anyhow!(
                    "Invalid IPN EID components: {}, {}, {}",
                    v1,
                    v2,
                    v3
                ));
            }

            // Check indefinite array length
            if value.count().is_none() {
                value.parse_end_or_else(|| anyhow!("Additional items found in IPN EID array"))?;
            }

            (3, v1 as u32, v2 as u32, v3 as u32)
        } else {
            if v2 >= 2 ^ 32 {
                return Err(anyhow!("Invalid IPN EID service number {}", v2));
            }
            (
                2,
                (v1 >> 32) as u32,
                (v1 & ((2 ^ 32) - 1)) as u32,
                v2 as u32,
            )
        };

        if allocator_id == 0 && node_number == 0 {
            if service_number != 0 {
                log::info!("Null EID with service number {}", service_number)
            }
            Ok(Self::Null)
        } else if allocator_id == 0 && node_number == (2 ^ 32) - 1 {
            Ok(Self::LocalNode { service_number })
        } else if components == 2 {
            Ok(Self::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            })
        } else {
            Ok(Self::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            })
        }
    }
}

impl cbor::encode::ToCbor for &Eid {
    fn to_cbor(self, tags: &[u64]) -> Vec<u8> {
        cbor::encode::emit_with_tags(
            match self {
                Eid::Null => [cbor::encode::emit(1u8), cbor::encode::emit(0u8)],
                Eid::Dtn { node_name, demux } => [
                    cbor::encode::emit(1u8),
                    cbor::encode::emit(["/", node_name.as_str(), demux.as_str()].join("/")),
                ],
                Eid::Ipn2 {
                    allocator_id,
                    node_number,
                    service_number,
                } => [
                    cbor::encode::emit(2u8),
                    cbor::encode::emit([
                        cbor::encode::emit((*allocator_id as u64) << 32 | *node_number as u64),
                        cbor::encode::emit(*service_number),
                    ]),
                ],
                Eid::Ipn3 {
                    allocator_id: 0,
                    node_number,
                    service_number,
                } => [
                    cbor::encode::emit(2u8),
                    cbor::encode::emit([
                        cbor::encode::emit(*node_number),
                        cbor::encode::emit(*service_number),
                    ]),
                ],
                Eid::Ipn3 {
                    allocator_id,
                    node_number,
                    service_number,
                } => [
                    cbor::encode::emit(2u8),
                    cbor::encode::emit([
                        cbor::encode::emit(*allocator_id),
                        cbor::encode::emit(*node_number),
                        cbor::encode::emit(*service_number),
                    ]),
                ],
                Eid::LocalNode { service_number } => [
                    cbor::encode::emit(2u8),
                    cbor::encode::emit([
                        cbor::encode::emit((2u64 ^ 32) - 1),
                        cbor::encode::emit(*service_number),
                    ]),
                ],
            },
            tags,
        )
    }
}

impl cbor::decode::FromCbor for Eid {
    fn from_cbor(data: &[u8]) -> Result<(Self, usize, Vec<u64>), anyhow::Error> {
        cbor::decode::parse_value(data, |value, tags| {
            if let cbor::decode::Value::Array(mut a) = value {
                match a.count() {
                    None => log::info!("Parsing EID array of indefinite length"),
                    Some(count) if count != 2 => {
                        return Err(anyhow!("EID is not encoded as a 2 element CBOR array"))
                    }
                    _ => {}
                }
                let schema = a.parse::<u64>()?;
                let (eid, _) = a.parse_value(|value: cbor::decode::Value<'_>, _, tags2| {
                    if !tags2.is_empty() {
                        log::info!("Parsing EID value with tags");
                    }
                    match (schema, value) {
                        (1, value) => Self::parse_dtn_eid(value),
                        (2, cbor::decode::Value::Array(a)) => Self::parse_ipn_eid(a),
                        (2, _) => Err(anyhow!("IPN EIDs must be encoded as a CBOR array")),
                        _ => Err(anyhow!("Unsupported EID scheme {}", schema)),
                    }
                })?;

                if a.count().is_none() {
                    a.parse_end_or_else(|| anyhow!("Additional items found in EID array"))?;
                }
                Ok((eid, tags.to_vec()))
            } else {
                Err(anyhow!("EID is not encoded as a CBOR array"))
            }
        })
        .map(|((eid, tags), len)| (eid, len, tags))
    }
}

impl std::str::FromStr for Eid {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Some(s) = s.strip_prefix("dtn://") {
            if !s.is_ascii() {
                Err(anyhow!("dtn URI be ASCII"))
            } else if let Some((s1, s2)) = s.split_once('/') {
                if s1.is_empty() {
                    Err(anyhow!("dtn URI node-name is empty"))
                } else {
                    Ok(Self::Dtn {
                        node_name: s1.to_string(),
                        demux: s2.to_string(),
                    })
                }
            } else {
                Err(anyhow!("dtn URI missing name-delim '/'"))
            }
        } else if let Some(s) = s.strip_prefix("ipn:") {
            let mut parts = s.split('.');
            if let Some(value) = parts.next() {
                let v1 = value.parse::<u32>()?;
                if let Some(value) = &parts.next() {
                    let v2 = value.parse::<u32>()?;
                    if let Some(value) = &parts.next() {
                        let v3 = value.parse::<u32>()?;
                        if parts.next().is_some() {
                            Err(anyhow!("Invalid ipn URI"))
                        } else {
                            Ok(Self::Ipn3 {
                                allocator_id: v1,
                                node_number: v2,
                                service_number: v3,
                            })
                        }
                    } else {
                        Ok(Self::Ipn3 {
                            allocator_id: 0,
                            node_number: v1,
                            service_number: v2,
                        })
                    }
                } else {
                    Err(anyhow!("Invalid ipn URI"))
                }
            } else {
                Err(anyhow!("Invalid ipn URI"))
            }
        } else {
            Err(anyhow!("EID has unsupported scheme"))
        }
    }
}

impl std::fmt::Display for Eid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Eid::Null => f.write_str("ipn:0.0"),
            Eid::LocalNode { service_number } => {
                f.write_fmt(format_args!("ipn:!.{service_number}",))
            }
            Eid::Ipn2 {
                allocator_id: 0,
                node_number,
                service_number,
            } => f.write_fmt(format_args!("ipn:{node_number}.{service_number}")),
            Eid::Ipn3 {
                allocator_id: 0,
                node_number,
                service_number,
            } => f.write_fmt(format_args!("ipn:{node_number}.{service_number}")),
            Eid::Ipn2 {
                allocator_id,
                node_number,
                service_number,
            } => f.write_fmt(format_args!(
                "ipn:{allocator_id}.{node_number}.{service_number}"
            )),
            Eid::Ipn3 {
                allocator_id,
                node_number,
                service_number,
            } => f.write_fmt(format_args!(
                "ipn:{allocator_id}.{node_number}.{service_number}"
            )),
            Eid::Dtn { node_name, demux } => f.write_fmt(format_args!("dtn://{node_name}/{demux}")),
        }
    }
}
