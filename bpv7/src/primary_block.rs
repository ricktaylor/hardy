/*!
The BPv7 primary block ([RFC 9171] §4.2). Defines the [`PrimaryBlock`]
type — the bundle's identifying header (source/destination/report-to EIDs,
creation timestamp, lifetime, fragment info, CRC) — together with its CBOR
decode and emit. Reachable from [`Bundle::primary`](crate::bundle::Bundle).

[RFC 9171]: https://www.rfc-editor.org/rfc/rfc9171.html
*/

use super::*;
use error::CaptureFieldErr;
use hardy_cbor::decode::Error as CborError;

#[derive(Clone, Debug)]
pub struct PrimaryBlock {
    pub flags: bundle::Flags,
    pub id: bundle::Id,
    pub crc_type: crc::CrcType,
    pub destination: eid::Eid,
    pub report_to: eid::Eid,
    pub lifetime: core::time::Duration,
}

impl PrimaryBlock {
    /// Emit a primary block as a CBOR-encoded `Vec<u8>`. Method on
    /// `PrimaryBlock` — callers that have a `Bundle` pass
    /// `bundle.primary.emit()`; builder constructs a `PrimaryBlock`
    /// from its fields.
    pub fn emit(&self) -> Result<Vec<u8>, Error> {
        crc::append_crc_value(
            self.crc_type,
            hardy_cbor::encode::emit_array(
                Some({
                    let mut count = if matches!(self.crc_type, crc::CrcType::None) {
                        8
                    } else {
                        9
                    };
                    if self.id.fragment_info.is_some() {
                        count += 2;
                    }
                    count
                }),
                |a| {
                    a.emit(&7);
                    a.emit(&self.flags);
                    a.emit(&self.crc_type);
                    a.emit(&self.destination);
                    a.emit(&self.id.source);
                    a.emit(&self.report_to);
                    a.emit(&self.id.timestamp);
                    a.emit(&(self.lifetime.as_millis() as u64));

                    // Fragment info
                    if let Some(fragment_info) = &self.id.fragment_info {
                        a.emit(&fragment_info.offset);
                        a.emit(&fragment_info.total_adu_length);
                    }

                    // CRC
                    if !matches!(self.crc_type, crc::CrcType::None) {
                        a.skip_value();
                    }
                },
            ),
        )
        .map_err(Into::into)
    }
}

impl hardy_cbor::decode::FromCbor for PrimaryBlock {
    type Error = Error;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |block, s, tags| {
            // RFC 9171 §4.1: indefinite-length items are not prohibited for
            // any field. Tags are still rejected (no RFC carve-out for those).
            // The canonical flag accumulates across all fields so BPSec callers
            // can detect and re-emit a canonical form when needed (RFC 9172 §4).
            if !s || !tags.is_empty() {
                return Err(Error::NotCanonical);
            }
            // `s` tracks non-shortest-length definite encodings (rejected above),
            // not definiteness. Use is_definite() to seed canonical so indefinite
            // outer arrays are flagged; AND'd with array-type fields below.
            let mut canonical = block.is_definite();

            // Version: always 7; enforce canonical encoding of the integer.
            let (version, v_s): (u64, bool) =
                block.parse().map_field_err::<Error>("version")?;
            if !v_s {
                return Err(Error::NotCanonical);
            }
            if version != 7 {
                return Err(Error::InvalidVersion(version));
            }

            // Newtypes (Flags, CrcType) self-enforce canonical in their own from_cbor.
            let flags: bundle::Flags =
                parse::parse_canonical_item(block, "bundle processing control flags")?;
            let crc_type: crc::CrcType = parse::parse_canonical_item(block, "crc type")?;

            // EIDs and timestamp are CBOR arrays; RFC 9171 §4.1 permits
            // indefinite-length encoding. Use parse_item and accumulate the flag.
            let (destination, dest_s) =
                parse::parse_item::<eid::Eid>(block, "destination endpoint id")?;
            let (source, src_s) =
                parse::parse_item::<eid::Eid>(block, "source endpoint id")?;
            let (report_to, rpt_s) =
                parse::parse_item::<eid::Eid>(block, "report-to endpoint id")?;
            let (timestamp, ts_s) =
                parse::parse_item::<creation_timestamp::CreationTimestamp>(block, "timestamp")?;
            canonical &= dest_s & src_s & rpt_s & ts_s;

            let lifetime = core::time::Duration::from_millis(parse::parse_canonical_item::<u64>(
                block, "lifetime",
            )?);

            // Parse fragment parts
            let fragment_info = if !flags.is_fragment {
                None
            } else {
                let offset = parse::parse_canonical_item::<u64>(block, "fragment offset")?;
                let total_adu_length =
                    parse::parse_canonical_item::<u64>(block, "total adu length")?;
                if offset > total_adu_length {
                    return Err(Error::InvalidFragmentInfo(offset, total_adu_length));
                }
                Some(bundle::FragmentInfo {
                    offset,
                    total_adu_length,
                })
            };

            // Parse the CRC value (or its absence) out of the block array,
            // then drive a Digest over the block bytes to verify it.
            (|| -> Result<(), Error> {
                let crc_start = block.offset();
                let crc_value = block.try_parse_value(|value, s, tags| {
                    if !s || !tags.is_empty() {
                        return Err(Error::NotCanonical);
                    }
                    if let hardy_cbor::decode::Value::Bytes(crc) = value {
                        Ok(crc.start + crc_start..crc.end + crc_start)
                    } else {
                        Err(Error::InvalidCBOR(CborError::IncorrectType(
                            "Definite-length Byte String".to_string(),
                            value.type_name(!tags.is_empty()),
                        )))
                    }
                })?;
                block.at_end()?;
                let crc_end = block.offset();

                match (crc_type, crc_value) {
                    (crc::CrcType::None, None) => Ok(()),
                    (crc::CrcType::None, Some(_)) => Err(crc::Error::UnexpectedCrcValue.into()),
                    (_, None) => Err(crc::Error::MissingCrc.into()),
                    (crc_type, Some(crc)) => {
                        let mut digest = crc::Digest::new(crc_type)?;
                        digest.push(&data[0..crc.start]);
                        digest.push_zeros();
                        digest.push(&data[crc.end..crc_end]);
                        if !digest.verify(&data[crc.start..crc.end]) {
                            return Err(crc::Error::IncorrectCrc.into());
                        }
                        Ok(())
                    }
                }
            })()
            .map_field_err::<Error>("CRC value")?;

            Ok((
                PrimaryBlock {
                    flags,
                    id: bundle::Id {
                        source,
                        timestamp,
                        fragment_info,
                    },
                    crc_type,
                    destination,
                    report_to,
                    lifetime,
                },
                canonical,
            ))
        })
        .map(|((v, s), len)| (v, s, len))
    }
}

impl PrimaryBlock {
    /// Build a [`block::Block`] entry for a freshly-emitted primary
    /// block. `extent` is the absolute byte range in the bundle's wire
    /// stream — caller must pass the `Range` returned by the CBOR
    /// encoder's `emit` call (the primary block sits after the outer
    /// `0x9F` array head, so it's NOT at offset 0).
    ///
    /// `data` is set to `0..extent.len()` per the primary-block
    /// convention (no inner CBOR wrapper — the whole primary block IS
    /// the data, head byte included).
    pub fn as_block(crc_type: crc::CrcType, extent: core::ops::Range<usize>) -> block::Block {
        let len = extent.len() as u64;
        block::Block {
            block_type: block::Type::Primary,
            flags: block::Flags::primary(),
            crc_type,
            data: 0..len,
            extent: extent.start as u64..extent.end as u64,
            ..Default::default()
        }
    }
}
