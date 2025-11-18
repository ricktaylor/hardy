/*!
This internal module handles the parsing and emission of the BPv7 Primary Block.
It defines an intermediate `PrimaryBlock` struct that is used during the CBOR
decoding process before the final `Bundle` struct is assembled.
*/

use super::*;
use error::CaptureFieldErr;

/// An intermediate representation of the Primary Block used during parsing.
///
/// This struct holds the results of parsing each field of the primary block.
/// Since parsing can fail at any point, most fields are stored as `Result` types
/// to capture potential errors without halting the entire parsing process immediately.
/// This allows for more graceful error handling and the ability to construct a partial
/// `Bundle` for debugging or status reporting even if the primary block is malformed.
pub struct PrimaryBlock {
    /// The parsed bundle processing control flags.
    pub flags: bundle::Flags,
    /// The result of parsing the CRC type.
    pub crc_type: Result<crc::CrcType, Error>,
    /// The result of parsing the source EID.
    pub source: Result<eid::Eid, Error>,
    /// The result of parsing the destination EID.
    pub destination: Result<eid::Eid, Error>,
    /// The parsed report-to EID.
    pub report_to: eid::Eid,
    /// The result of parsing the creation timestamp.
    pub timestamp: Result<creation_timestamp::CreationTimestamp, Error>,
    /// The result of parsing the bundle lifetime.
    pub lifetime: Result<core::time::Duration, Error>,
    /// The result of parsing the fragmentation information.
    pub fragment_info: Result<Option<bundle::FragmentInfo>, Error>,
    /// The result of the CRC validation.
    pub crc_result: Result<(), Error>,
}

impl hardy_cbor::decode::FromCbor for PrimaryBlock {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |block, s, tags| {
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
                    (Ok((offset, s1)), Ok((total_adu_length, s2))) => {
                        if offset >= total_adu_length {
                            Err(Error::InvalidFragmentInfo(offset, total_adu_length))
                        } else {
                            shortest = shortest && s1 && s2;
                            Ok(Some(bundle::FragmentInfo {
                                offset,
                                total_adu_length,
                            }))
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
        .map(|((v, s), len)| (v, s, len))
    }
}

impl PrimaryBlock {
    pub fn as_block(crc_type: crc::CrcType, extent: core::ops::Range<usize>) -> block::Block {
        block::Block {
            block_type: block::Type::Primary,
            flags: block::Flags::default(),
            crc_type,
            data: 0..extent.len(),
            extent,
            bib: None,
            bcb: None,
        }
    }

    /// Converts the intermediate `PrimaryBlock` into a `bundle::Bundle`.
    ///
    /// This method constructs a `Bundle` from the parsed fields. If any field
    /// failed to parse, a default value is used for that field in the `Bundle`,
    /// and the first error encountered is returned as an `Option<Error>`.
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
        let crc_type = unpack(self.crc_type, &mut e, "Crc Type");
        let bundle = bundle::Bundle {
            flags: self.flags,
            report_to: self.report_to,
            destination: unpack(self.destination, &mut e, "Destination EID"),
            id: bundle::Id {
                source: unpack(self.source, &mut e, "Source EID"),
                timestamp: unpack(self.timestamp, &mut e, "Creation Timestamp"),
                fragment_info: unpack(self.fragment_info, &mut e, "Fragment Info"),
            },
            lifetime: unpack(self.lifetime, &mut e, "Lifetime"),
            crc_type,
            blocks: [(0, Self::as_block(crc_type, extent))].into(),
            ..Default::default()
        };

        let e = e
            .or_else(|| {
                self.crc_result
                    .map_err(|e| Error::InvalidField {
                        field: "Crc Value",
                        source: e.into(),
                    })
                    .err()
            })
            .or_else(|| {
                (
                    // TODO: Null Report-To EID ?!?!

                    // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-5
                    (bundle.id.source.is_null()
                        && (bundle.flags.is_fragment
                            || !bundle.flags.do_not_fragment
                            || bundle.flags.receipt_report_requested
                            || bundle.flags.forward_report_requested
                            || bundle.flags.delivery_report_requested
                            || bundle.flags.delete_report_requested))

                    // Invalid flag combination https://www.rfc-editor.org/rfc/rfc9171.html#section-4.2.3-4
                    || (bundle.flags.is_admin_record
                        && (bundle.flags.receipt_report_requested
                            || bundle.flags.forward_report_requested
                            || bundle.flags.delivery_report_requested
                            || bundle.flags.delete_report_requested))
                )
                    .then_some(Error::InvalidFlags)
            });

        (bundle, e)
    }

    /// Emits a `PrimaryBlock` into a CBOR-encoded `Vec<u8>`.
    ///
    /// This function is used during bundle creation to serialize the primary block
    /// based on the data in a `bundle::Bundle` struct.
    pub fn emit(bundle: &bundle::Bundle) -> Result<Vec<u8>, Error> {
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
                        a.emit(&fragment_info.total_adu_length);
                    }

                    // CRC
                    if let crc::CrcType::None = bundle.crc_type {
                    } else {
                        a.skip_value();
                    }
                },
            ),
        )
        .map_err(Into::into)
    }
}
