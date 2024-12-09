use super::*;
use error::CaptureFieldErr;

struct PartialPrimaryBlock {
    pub flags: BundleFlags,
    pub crc_type: Result<CrcType, Error>,
    pub source: Result<Eid, Error>,
    pub destination: Result<Eid, Error>,
    pub report_to: Eid,
    pub timestamp: Result<CreationTimestamp, Error>,
    pub lifetime: Result<u64, Error>,
    pub fragment_info: Result<Option<FragmentInfo>, Error>,
    pub crc_result: Result<(), Error>,
}

impl cbor::decode::FromCbor for PartialPrimaryBlock {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        cbor::decode::try_parse_array(data, |block, s, tags| {
            let mut shortest = s && tags.is_empty() && block.is_definite();

            // Check version
            let version = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("version")?;
            if version != 7 {
                return Err(Error::InvalidVersion(version));
            }

            // Parse flags
            let flags = block
                .parse::<(BundleFlags, bool)>()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("bundle processing control flags")?;

            // Parse CRC Type
            let crc_type = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_err(Into::into);

            // Parse EIDs
            let destination = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_err(Into::into);

            let source = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_err(Into::into);

            let report_to = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("report-to EID")?;

            // Parse timestamp
            let timestamp = block.parse().map(|(v, s)| {
                shortest = shortest && s;
                v
            });

            // Parse lifetime
            let lifetime = block
                .parse()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_err(Into::into);

            // Parse fragment parts
            let fragment_info = if !flags.is_fragment {
                Ok(None)
            } else {
                match (block.parse(), block.parse()) {
                    (Ok((offset, s1)), Ok((total_len, s2))) => {
                        shortest = shortest && s1 && s2;
                        Ok(Some(FragmentInfo { offset, total_len }))
                    }
                    (Err(e), _) => Err(e.into()),
                    (_, Err(e)) => Err(e.into()),
                }
            };

            // Try to parse and check CRC
            let crc_result = match &crc_type {
                Ok(crc_type) => crc::parse_crc_value(data, block, *crc_type)
                    .map(|s| {
                        shortest = shortest && s;
                    })
                    .map_err(Into::into),
                Err(_) => Ok(()),
            };

            Ok((
                Self {
                    flags,
                    crc_type,
                    source,
                    destination,
                    report_to,
                    lifetime,
                    timestamp,
                    fragment_info,
                    crc_result,
                },
                shortest,
            ))
        })
        .map(|o| o.map(|((v, s), len)| (v, s, len)))
    }
}

pub struct PrimaryBlock {
    pub flags: BundleFlags,
    pub crc_type: CrcType,
    pub source: Eid,
    pub destination: Eid,
    pub report_to: Eid,
    pub timestamp: CreationTimestamp,
    pub lifetime: u64,
    pub fragment_info: Option<FragmentInfo>,
    pub error: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl PrimaryBlock {
    pub fn into_bundle(self) -> (Bundle, Option<Box<dyn std::error::Error + Send + Sync>>) {
        (
            Bundle {
                id: BundleId {
                    source: self.source,
                    timestamp: self.timestamp,
                    fragment_info: self.fragment_info,
                },
                flags: self.flags,
                crc_type: self.crc_type,
                destination: self.destination,
                report_to: self.report_to,
                lifetime: self.lifetime,
                ..Default::default()
            },
            self.error,
        )
    }

    pub fn emit(bundle: &Bundle) -> Vec<u8> {
        crc::append_crc_value(
            bundle.crc_type,
            cbor::encode::emit_array(
                Some({
                    let mut count = if let CrcType::None = bundle.crc_type {
                        8
                    } else {
                        9
                    };
                    if bundle.id.fragment_info.is_some() {
                        count += 2;
                    }
                    count
                }),
                |a| {
                    a.emit(7);
                    a.emit(&bundle.flags);
                    a.emit(bundle.crc_type);
                    a.emit(&bundle.destination);
                    a.emit(&bundle.id.source);
                    a.emit(&bundle.report_to);
                    a.emit(&bundle.id.timestamp);
                    a.emit(bundle.lifetime);

                    // Fragment info
                    if let Some(fragment_info) = &bundle.id.fragment_info {
                        a.emit(fragment_info.offset);
                        a.emit(fragment_info.total_len);
                    }

                    // CRC
                    if let CrcType::None = bundle.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        )
    }
}

impl cbor::decode::FromCbor for PrimaryBlock {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        let Some((p, s, len)) =
            cbor::decode::try_parse::<(PartialPrimaryBlock, bool, usize)>(data)?
        else {
            return Ok(None);
        };

        // Compose something out of what we have!
        Ok(Some((
            match (
                p.destination,
                p.source,
                p.timestamp,
                p.lifetime,
                p.fragment_info,
                p.crc_type,
                p.crc_result,
            ) {
                (
                    Ok(destination),
                    Ok(source),
                    Ok(timestamp),
                    Ok(lifetime),
                    Ok(fragment_info),
                    Ok(crc_type),
                    Ok(()),
                ) => {
                    let mut block = Self {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type,
                        source,
                        destination,
                        timestamp,
                        lifetime,
                        fragment_info,
                        error: None,
                    };

                    // Check flags
                    if matches!(&block.source,&Eid::Null if block.flags.is_fragment
                        || !block.flags.do_not_fragment
                        || block.flags.receipt_report_requested
                        || block.flags.forward_report_requested
                        || block.flags.delivery_report_requested
                        || block.flags.delete_report_requested)
                    {
                        // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-5
                        block.error = Some(Error::InvalidFlags.into());
                    } else if block.flags.is_admin_record
                        && (block.flags.receipt_report_requested
                            || block.flags.forward_report_requested
                            || block.flags.delivery_report_requested
                            || block.flags.delete_report_requested)
                    {
                        // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-4
                        block.error = Some(Error::InvalidFlags.into());
                    }
                    block
                }
                (Err(e), source, timestamp, lifetime, fragment_info, crc_type, _) => PrimaryBlock {
                    flags: p.flags,
                    report_to: p.report_to,
                    crc_type: crc_type.unwrap_or_default(),
                    source: source.unwrap_or_default(),
                    destination: Eid::default(),
                    timestamp: timestamp.unwrap_or_default(),
                    lifetime: lifetime.unwrap_or(0),
                    fragment_info: fragment_info.unwrap_or_default(),
                    error: Some(
                        Error::InvalidField {
                            field: "Destination EID",
                            source: e.into(),
                        }
                        .into(),
                    ),
                },
                (Ok(destination), Err(e), timestamp, lifetime, fragment_info, crc_type, _) => {
                    PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source: Eid::default(),
                        destination,
                        timestamp: timestamp.unwrap_or_default(),
                        lifetime: lifetime.unwrap_or(0),
                        fragment_info: fragment_info.unwrap_or_default(),
                        error: Some(
                            Error::InvalidField {
                                field: "Source EID",
                                source: e.into(),
                            }
                            .into(),
                        ),
                    }
                }
                (Ok(destination), Ok(source), Err(e), lifetime, fragment_info, crc_type, _) => {
                    PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source,
                        destination,
                        timestamp: CreationTimestamp::default(),
                        lifetime: lifetime.unwrap_or(0),
                        fragment_info: fragment_info.unwrap_or_default(),
                        error: Some(
                            Error::InvalidField {
                                field: "Creation Timestamp",
                                source: e.into(),
                            }
                            .into(),
                        ),
                    }
                }
                (
                    Ok(destination),
                    Ok(source),
                    Ok(timestamp),
                    Err(e),
                    fragment_info,
                    crc_type,
                    _,
                ) => PrimaryBlock {
                    flags: p.flags,
                    report_to: p.report_to,
                    crc_type: crc_type.unwrap_or_default(),
                    source,
                    destination,
                    timestamp,
                    lifetime: 0,
                    fragment_info: fragment_info.unwrap_or_default(),
                    error: Some(
                        Error::InvalidField {
                            field: "Lifetime",
                            source: e.into(),
                        }
                        .into(),
                    ),
                },
                (Ok(destination), Ok(source), Ok(timestamp), Ok(lifetime), Err(e), crc_type, _) => {
                    PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source,
                        destination,
                        timestamp,
                        lifetime,
                        fragment_info: None,
                        error: Some(
                            Error::InvalidField {
                                field: "Fragment Info",
                                source: e.into(),
                            }
                            .into(),
                        ),
                    }
                }
                (
                    Ok(destination),
                    Ok(source),
                    Ok(timestamp),
                    Ok(lifetime),
                    Ok(fragment_info),
                    Err(e),
                    _,
                ) => PrimaryBlock {
                    flags: p.flags,
                    report_to: p.report_to,
                    crc_type: CrcType::default(),
                    source,
                    destination,
                    timestamp,
                    lifetime,
                    fragment_info,
                    error: Some(
                        Error::InvalidField {
                            field: "CRC Type",
                            source: e.into(),
                        }
                        .into(),
                    ),
                },
                (
                    Ok(destination),
                    Ok(source),
                    Ok(timestamp),
                    Ok(lifetime),
                    Ok(fragment_info),
                    Ok(crc_type),
                    Err(e),
                ) => PrimaryBlock {
                    flags: p.flags,
                    report_to: p.report_to,
                    crc_type,
                    source,
                    destination,
                    timestamp,
                    lifetime,
                    fragment_info,
                    error: Some(
                        Error::InvalidField {
                            field: "CRC Value",
                            source: e.into(),
                        }
                        .into(),
                    ),
                },
            },
            s,
            len,
        )))
    }
}
