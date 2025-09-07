use super::*;
use error::CaptureFieldErr;

pub struct PrimaryBlock {
    pub flags: bundle::Flags,
    pub crc_type: Result<crc::CrcType, Error>,
    pub source: Result<eid::Eid, Error>,
    pub destination: Result<eid::Eid, Error>,
    pub report_to: eid::Eid,
    pub timestamp: Result<creation_timestamp::CreationTimestamp, Error>,
    pub lifetime: Result<core::time::Duration, Error>,
    pub fragment_info: Result<Option<bundle::FragmentInfo>, Error>,
    pub crc_result: Result<(), Error>,
}

impl hardy_cbor::decode::FromCbor for PrimaryBlock {
    type Error = Error;

    fn try_from_cbor(data: &[u8]) -> Result<Option<(Self, bool, usize)>, Self::Error> {
        hardy_cbor::decode::try_parse_array(data, |block, s, tags| {
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

            // Parse flags - we must have some readable flags
            let flags = block
                .parse::<(bundle::Flags, bool)>()
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

            // We must have a valid report_to
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
                .parse::<(u64, bool)>()
                .map(|(v, s)| {
                    shortest = shortest && s;
                    core::time::Duration::from_millis(v)
                })
                .map_err(Into::into);

            // Parse fragment parts
            let fragment_info = if !flags.is_fragment {
                Ok(None)
            } else {
                match (block.parse(), block.parse()) {
                    (Ok((offset, s1)), Ok((total_len, s2))) => {
                        if offset >= total_len {
                            Err(Error::InvalidFragmentInfo(offset, total_len))
                        } else {
                            shortest = shortest && s1 && s2;
                            Ok(Some(bundle::FragmentInfo { offset, total_len }))
                        }
                    }
                    (Err(e), _) | (_, Err(e)) => Err(e.into()),
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

impl PrimaryBlock {
    pub fn into_bundle(self, extent: core::ops::Range<usize>) -> (bundle::Bundle, Option<Error>) {
        // Unpack the value or default
        fn unpack<T: core::default::Default>(
            r: Result<T, Error>,
            e: &mut Option<Error>,
            field: &'static str,
        ) -> T {
            match r {
                Ok(t) => t,
                Err(e2) => {
                    if e.is_none() {
                        *e = Some(Error::InvalidField {
                            field,
                            source: e2.into(),
                        });
                    }
                    T::default()
                }
            }
        }

        // Compose something out of what we have!
        let mut e = None;
        let mut bundle = bundle::Bundle {
            flags: self.flags,
            report_to: self.report_to,
            destination: unpack(self.destination, &mut e, "Destination EID"),
            id: bundle::Id {
                source: unpack(self.source, &mut e, "Source EID"),
                timestamp: unpack(self.timestamp, &mut e, "Creation Timestamp"),
                fragment_info: unpack(self.fragment_info, &mut e, "Fragment Info"),
            },
            lifetime: unpack(self.lifetime, &mut e, "Lifetime"),
            crc_type: unpack(self.crc_type, &mut e, "Crc Type"),
            ..Default::default()
        };

        if e.is_none()
            && let Err(e2) = self.crc_result
        {
            e = Some(Error::InvalidField {
                field: "Crc Value",
                source: e2.into(),
            });
        }

        // Add a block 0
        bundle.blocks.insert(
            0,
            block::Block {
                block_type: block::Type::Primary,
                flags: block::Flags {
                    must_replicate: true,
                    report_on_failure: true,
                    delete_bundle_on_failure: true,
                    ..Default::default()
                },
                crc_type: bundle.crc_type,
                data: extent.clone(),
                extent,
                bib: None,
                bcb: None,
            },
        );

        if e.is_none() {
            // Check flags
            if matches!(&bundle.id.source,&eid::Eid::Null if bundle.flags.is_fragment
                            || !bundle.flags.do_not_fragment
                            || bundle.flags.receipt_report_requested
                            || bundle.flags.forward_report_requested
                            || bundle.flags.delivery_report_requested
                            || bundle.flags.delete_report_requested)
            {
                // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-5
                e = Some(Error::InvalidFlags);
            } else if bundle.flags.is_admin_record
                && (bundle.flags.receipt_report_requested
                    || bundle.flags.forward_report_requested
                    || bundle.flags.delivery_report_requested
                    || bundle.flags.delete_report_requested)
            {
                // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-4
                e = Some(Error::InvalidFlags);
            }
        }

        (bundle, e)
    }

    pub fn emit(bundle: &bundle::Bundle) -> Vec<u8> {
        crc::append_crc_value(
            bundle.crc_type,
            hardy_cbor::encode::emit_array(
                Some({
                    let mut count = if let crc::CrcType::None = bundle.crc_type {
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
                    a.emit(&7);
                    a.emit(&bundle.flags);
                    a.emit(&bundle.crc_type);
                    a.emit(&bundle.destination);
                    a.emit(&bundle.id.source);
                    a.emit(&bundle.report_to);
                    a.emit(&bundle.id.timestamp);
                    a.emit(&(bundle.lifetime.as_millis() as u64));

                    // Fragment info
                    if let Some(fragment_info) = &bundle.id.fragment_info {
                        a.emit(&fragment_info.offset);
                        a.emit(&fragment_info.total_len);
                    }

                    // CRC
                    if let crc::CrcType::None = bundle.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        )
    }
}
