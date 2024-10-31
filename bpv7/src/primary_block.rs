use super::*;
use bundle::CaptureFieldErr;

struct PartialPrimaryBlock {
    pub flags: BundleFlags,
    pub crc_type: Result<CrcType, BundleError>,
    pub source: Result<Eid, BundleError>,
    pub destination: Result<Eid, BundleError>,
    pub report_to: Eid,
    pub timestamp: Result<CreationTimestamp, BundleError>,
    pub lifetime: Result<u64, BundleError>,
    pub fragment_info: Result<Option<FragmentInfo>, BundleError>,
    pub crc_result: Result<(), BundleError>,
}

impl cbor::decode::FromCbor for PartialPrimaryBlock {
    type Error = BundleError;

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
                .map_field_err("Version")?;
            if version != 7 {
                return Err(BundleError::UnsupportedVersion(version));
            }

            // Parse flags
            let flags = block
                .parse::<(BundleFlags, bool)>()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    v
                })
                .map_field_err("Bundle Processing Control Flags")?;

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
                .map_field_err("Report-to EID")?;

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
}

impl PrimaryBlock {
    pub fn into_bundle(self) -> Bundle {
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
        }
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
                |a, _| {
                    // Version
                    a.emit(7);
                    // Flags
                    a.emit(&bundle.flags);
                    // CRC
                    a.emit(bundle.crc_type);
                    // EIDs
                    a.emit(&bundle.destination);
                    a.emit(&bundle.id.source);
                    a.emit(&bundle.report_to);
                    // Timestamp
                    a.emit(&bundle.id.timestamp);
                    // Lifetime
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
    type Error = BundleError;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        let Some((p, s, len)) =
            cbor::decode::try_parse::<(PartialPrimaryBlock, bool, usize)>(data)?
        else {
            return Ok(None);
        };

        // Compose something out of what we have!
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
                let block = Self {
                    flags: p.flags,
                    report_to: p.report_to,
                    crc_type,
                    source,
                    destination,
                    timestamp,
                    lifetime,
                    fragment_info,
                };

                // Check flags
                if let &Eid::Null = &block.source {
                    if block.flags.is_fragment
                        || !block.flags.do_not_fragment
                        || block.flags.receipt_report_requested
                        || block.flags.forward_report_requested
                        || block.flags.delivery_report_requested
                        || block.flags.delete_report_requested
                    {
                        // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-5
                        return Err(BundleError::InvalidBundle {
                            bundle: block.into_bundle().into(),
                            error: BundleError::InvalidFlags.into(),
                        });
                    }
                }

                if block.flags.is_admin_record
                    && (block.flags.receipt_report_requested
                        || block.flags.forward_report_requested
                        || block.flags.delivery_report_requested
                        || block.flags.delete_report_requested)
                {
                    // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-4
                    return Err(BundleError::InvalidBundle {
                        bundle: block.into_bundle().into(),
                        error: BundleError::InvalidFlags.into(),
                    });
                }

                Ok(Some((block, s, len)))
            }
            (Err(e), source, timestamp, lifetime, fragment_info, crc_type, _) => {
                Err(BundleError::InvalidBundle {
                    bundle: PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source: source.unwrap_or_default(),
                        destination: Eid::default(),
                        timestamp: timestamp.unwrap_or_default(),
                        lifetime: lifetime.unwrap_or(0),
                        fragment_info: fragment_info.unwrap_or_default(),
                    }
                    .into_bundle()
                    .into(),
                    error: e.into(),
                })
            }
            (Ok(destination), Err(e), timestamp, lifetime, fragment_info, crc_type, _) => {
                Err(BundleError::InvalidBundle {
                    bundle: PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source: Eid::default(),
                        destination,
                        timestamp: timestamp.unwrap_or_default(),
                        lifetime: lifetime.unwrap_or(0),
                        fragment_info: fragment_info.unwrap_or_default(),
                    }
                    .into_bundle()
                    .into(),
                    error: e.into(),
                })
            }
            (Ok(destination), Ok(source), Err(e), lifetime, fragment_info, crc_type, _) => {
                Err(BundleError::InvalidBundle {
                    bundle: PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source,
                        destination,
                        timestamp: CreationTimestamp::default(),
                        lifetime: lifetime.unwrap_or(0),
                        fragment_info: fragment_info.unwrap_or_default(),
                    }
                    .into_bundle()
                    .into(),
                    error: e.into(),
                })
            }
            (Ok(destination), Ok(source), Ok(timestamp), Err(e), fragment_info, crc_type, _) => {
                Err(BundleError::InvalidBundle {
                    bundle: PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source,
                        destination,
                        timestamp,
                        lifetime: 0,
                        fragment_info: fragment_info.unwrap_or_default(),
                    }
                    .into_bundle()
                    .into(),
                    error: e.into(),
                })
            }
            (Ok(destination), Ok(source), Ok(timestamp), Ok(lifetime), Err(e), crc_type, _) => {
                Err(BundleError::InvalidBundle {
                    bundle: PrimaryBlock {
                        flags: p.flags,
                        report_to: p.report_to,
                        crc_type: crc_type.unwrap_or_default(),
                        source,
                        destination,
                        timestamp,
                        lifetime,
                        fragment_info: None,
                    }
                    .into_bundle()
                    .into(),
                    error: e.into(),
                })
            }
            (
                Ok(destination),
                Ok(source),
                Ok(timestamp),
                Ok(lifetime),
                Ok(fragment_info),
                Err(e),
                _,
            ) => Err(BundleError::InvalidBundle {
                bundle: PrimaryBlock {
                    flags: p.flags,
                    report_to: p.report_to,
                    crc_type: CrcType::default(),
                    source,
                    destination,
                    timestamp,
                    lifetime,
                    fragment_info,
                }
                .into_bundle()
                .into(),
                error: e.into(),
            }),
            (
                Ok(destination),
                Ok(source),
                Ok(timestamp),
                Ok(lifetime),
                Ok(fragment_info),
                Ok(crc_type),
                Err(e),
            ) => Err(BundleError::InvalidBundle {
                bundle: PrimaryBlock {
                    flags: p.flags,
                    report_to: p.report_to,
                    crc_type,
                    source,
                    destination,
                    timestamp,
                    lifetime,
                    fragment_info,
                }
                .into_bundle()
                .into(),
                error: e.into(),
            }),
        }
    }
}
