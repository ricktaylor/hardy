use crate::EidPattern;
use crate::EidPatternItem;
use crate::IpnPatternItem;
use crate::Repr;
use crate::ipn_pattern::IpnInterval;
use crate::ipn_pattern::IpnPattern;
use alloc::vec::Vec;
use hardy_cbor::encode::Encoder;
use thiserror::Error;

// Implementation of CBOR serialization/parsing for EID patterns (currently only for IPN scheme).
// Follows the format from: https://datatracker.ietf.org/doc/html/draft-ietf-dtn-eid-pattern-11#section-2.4.4

// Errors produced by parsing EID patterns from CBOR.
#[derive(Error, Debug)]
pub enum PatternDecodeError {
    #[error(transparent)]
    InvalidCBOR(#[from] hardy_cbor::decode::Error),
    #[error("Unexpected scheme number: {0}")]
    UnexpectedSchemeNumber(u32),
    #[error("Encountered indefinite or empty array while trying to parse IPN range.")]
    IndefiniteOrEmptyArray,
}

impl hardy_cbor::encode::ToCbor for EidPattern {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        match &self.0 {
            Repr::Any => encoder.emit(&true),
            Repr::Set(n) => {
                encoder.emit_array(Some(n.len()), |arr| {
                    for eid in n {
                        arr.emit(eid);
                    }
                });
            }
        }
    }
}

impl hardy_cbor::decode::FromCbor for EidPattern {
    type Error = PatternDecodeError;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_value(data, |v, s, t| match v {
            hardy_cbor::decode::Value::True => Ok((EidPattern(Repr::Any), s)),
            hardy_cbor::decode::Value::Array(arr) => {
                let mut parsed_patterns: Vec<EidPatternItem> = Vec::from([]);
                while !arr.at_end()? {
                    let single_eid: EidPatternItem = arr.parse()?;
                    parsed_patterns.push(single_eid);
                }
                Ok((EidPattern(Repr::Set(parsed_patterns.into_boxed_slice())), s))
            }
            _ => Err(PatternDecodeError::InvalidCBOR(
                hardy_cbor::decode::Error::IncorrectType("True, Array", v.item_type(t)),
            )),
        })
        .map(|((value, shortest), len)| (value, shortest, len))
    }
}

impl hardy_cbor::encode::ToCbor for EidPatternItem {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        match self {
            // Could not derive from the specification how these are supposed to be encoded.
            EidPatternItem::AnyNumericScheme(_) => {
                unimplemented!()
            }
            EidPatternItem::AnyTextScheme(_) => {
                unimplemented!()
            }
            EidPatternItem::IpnPatternItem(ipi) => {
                encoder.emit_array(Some(2), |arr| {
                    arr.emit(&2);
                    arr.emit(&ipi);
                });
            }
            #[cfg(feature = "dtn-pat-item")]
            // The encoding for DTN-Scheme has not been specified yet.
            EidPatternItem::DtnPatternItem(_) => {
                unimplemented!()
            }
        }
    }
}

impl hardy_cbor::decode::FromCbor for EidPatternItem {
    type Error = PatternDecodeError;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |arr, s, _| {
            let x: u32 = arr.parse()?;
            match x {
                2 => Ok((EidPatternItem::IpnPatternItem(arr.parse()?), s)),
                n => Err(PatternDecodeError::UnexpectedSchemeNumber(n)),
            }
        })
        .map(|((value, shortest), len)| (value, shortest, len))
    }
}

impl hardy_cbor::encode::ToCbor for IpnPatternItem {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        encoder.emit_array(Some(3), |arr| {
            arr.emit(&self.allocator_id);
            arr.emit(&self.node_number);
            arr.emit(&self.service_number);
        });
    }
}

impl hardy_cbor::decode::FromCbor for IpnPatternItem {
    type Error = PatternDecodeError;

    fn from_cbor(data: &[u8]) -> Result<(Self, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_array(data, |arr, s, _| {
            Ok((
                IpnPatternItem {
                    allocator_id: arr.parse()?,
                    node_number: arr.parse()?,
                    service_number: arr.parse()?,
                },
                s,
            ))
        })
        .map(|((value, shortest), len)| (value, shortest, len))
    }
}

impl hardy_cbor::encode::ToCbor for IpnPattern {
    type Result = ();

    fn to_cbor(&self, encoder: &mut Encoder) -> Self::Result {
        match self {
            Self::Wildcard => {
                encoder.emit(&true);
            }
            Self::Single(uint) => {
                encoder.emit(&uint);
            }
            Self::Range(n) => {
                // NOTE: Separating the first interval, because it requires special handling.
                // Checking if the last interval open or closed.
                // The logic here could be cosmetically refined and simplified.
                let mut cbor_array_len = 2 * n.len();
                let mut last_range_closed = true;

                let last_interval = n.last().unwrap();
                if let IpnInterval::Range(r) = last_interval
                    && r.end().eq(&u32::MAX)
                {
                    cbor_array_len -= 1;
                    last_range_closed = false;
                    if cbor_array_len.eq(&1) {
                        encoder.emit_array(Some(1), |arr| {
                            arr.emit(&r.start());
                        });
                        return;
                    }
                }

                if let Some((first, intervals)) = n.split_first() {
                    encoder.emit_array(Some(cbor_array_len), |arr| {
                        let mut excluded_range: u32;
                        let mut previous_range_end;

                        match first {
                            IpnInterval::Number(x) => {
                                previous_range_end = *x;
                                arr.emit(&x);
                                arr.emit(&0);
                            }
                            IpnInterval::Range(r) => {
                                previous_range_end = *r.end();
                                arr.emit(&(r.start()));
                                arr.emit(&(r.end() - r.start()));
                            }
                        }

                        if let Some((last, preceding)) = intervals.split_last() {
                            for i in preceding {
                                match i {
                                    IpnInterval::Number(x) => {
                                        excluded_range = x - 1 - (previous_range_end + 1);
                                        arr.emit(&excluded_range);
                                        arr.emit(&0);
                                        previous_range_end = *x;
                                    }
                                    IpnInterval::Range(r) => {
                                        excluded_range = r.start() - 1 - (previous_range_end + 1);
                                        arr.emit(&excluded_range);
                                        arr.emit(&(r.end() - r.start()));
                                        previous_range_end = *r.end();
                                    }
                                }
                            }
                            match last {
                                IpnInterval::Number(x) => {
                                    excluded_range = x - 1 - (previous_range_end + 1);
                                    arr.emit(&excluded_range);
                                    arr.emit(&0);
                                }
                                IpnInterval::Range(r) => {
                                    excluded_range = r.start() - 1 - (previous_range_end + 1);
                                    arr.emit(&excluded_range);
                                    if last_range_closed {
                                        arr.emit(&(r.end() - r.start()));
                                    }
                                }
                            }
                        }
                    });
                };
            }
        }
    }
}

impl hardy_cbor::decode::FromCbor for IpnPattern {
    type Error = PatternDecodeError;

    fn from_cbor(data: &[u8]) -> Result<(IpnPattern, bool, usize), Self::Error> {
        hardy_cbor::decode::parse_value(data, |v, s, t| match v {
            hardy_cbor::decode::Value::True => Ok((IpnPattern::Wildcard, s)),
            hardy_cbor::decode::Value::UnsignedInteger(uint) => {
                Ok((IpnPattern::Single(uint as u32), s))
            }
            hardy_cbor::decode::Value::Array(arr) => {
                let arr_len = arr
                    .count()
                    .ok_or(PatternDecodeError::IndefiniteOrEmptyArray)?;
                if arr_len == 0 {
                    return Err(PatternDecodeError::IndefiniteOrEmptyArray);
                }
                if arr_len == 1 {
                    let first_v: u32 = arr.parse()?;
                    return Ok((
                        IpnPattern::Range(Vec::from([IpnInterval::Range(first_v..=u32::MAX)])),
                        s,
                    ));
                }
                if arr_len == 2 {
                    let first_v: u32 = arr.parse()?;
                    let second_v: u32 = arr.parse()?;
                    return Ok((
                        IpnPattern::Range(Vec::from([IpnInterval::Range(
                            first_v..=first_v + second_v,
                        )])),
                        s,
                    ));
                }
                // NOTE: Decoding each interval depends on the offset and previously decoded interval.
                // The code could be cosmetically refined and simplified.
                // arr_len > 2
                let mut parsed_ranges: Vec<IpnInterval> = Vec::from([]);

                let first_v: u32 = arr.parse()?;
                let second_v: u32 = arr.parse()?;
                let mut shift: u32;

                if second_v == 0 {
                    parsed_ranges.push(IpnInterval::Number(first_v));
                    shift = first_v;
                } else {
                    parsed_ranges.push(IpnInterval::Range(first_v..=first_v + second_v));
                    shift = first_v + second_v;
                }

                while !arr.at_end()? {
                    let first_v: u32 = arr.parse()?;
                    if arr.at_end()? {
                        parsed_ranges.push(IpnInterval::Range(first_v + 2 + shift..=u32::MAX));
                        break;
                    }
                    let second_v: u32 = arr.parse()?;

                    if second_v == 0 {
                        parsed_ranges.push(IpnInterval::Number(first_v + 2 + shift));
                        shift += first_v + 2;
                    } else {
                        parsed_ranges.push(IpnInterval::Range(
                            first_v + 2 + shift..=first_v + 2 + shift + second_v,
                        ));
                        shift += first_v + 2 + second_v;
                    }
                }
                Ok((IpnPattern::Range(parsed_ranges), s))
            }
            _ => Err(PatternDecodeError::InvalidCBOR(
                hardy_cbor::decode::Error::IncorrectType(
                    "True, UnsignedInteger, Array",
                    v.item_type(t),
                ),
            )),
        })
        .map(|((value, shortest), len)| (value, shortest, len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ranges_encoding() {
        let ipn_range_1 = IpnPattern::Range(Vec::from([IpnInterval::Range(0..=1)]));
        let ipn_range_2 = IpnPattern::Range(Vec::from([IpnInterval::Range(0..=4294967295)]));
        let ipn_range_3 = IpnPattern::Range(Vec::from([IpnInterval::Range(7..=4294967295)]));
        let ipn_range_4 = IpnPattern::Range(Vec::from([
            IpnInterval::Range(8..=9),
            IpnInterval::Number(11),
            IpnInterval::Range(23..=25),
        ]));
        let ipn_range_5 = IpnPattern::Range(Vec::from([
            IpnInterval::Number(1),
            IpnInterval::Range(7..=9),
            IpnInterval::Number(15),
        ]));
        let ipn_range_6 = IpnPattern::Range(Vec::from([
            IpnInterval::Number(1),
            IpnInterval::Range(7..=9),
            IpnInterval::Range(15..=4294967295),
        ]));
        let ipn_range_7 = IpnPattern::Range(Vec::from([
            IpnInterval::Number(1),
            IpnInterval::Range(7..=9),
            IpnInterval::Range(15..=19),
        ]));
        let ipn_range_8 = IpnPattern::Range(Vec::from([
            IpnInterval::Range(1..=2),
            IpnInterval::Range(7..=9),
            IpnInterval::Number(15),
        ]));
        let ipn_range_9 = IpnPattern::Range(Vec::from([
            IpnInterval::Range(1..=2),
            IpnInterval::Range(7..=9),
            IpnInterval::Range(15..=4294967295),
        ]));
        let ipn_range_10 = IpnPattern::Range(Vec::from([
            IpnInterval::Range(1..=2),
            IpnInterval::Range(7..=9),
            IpnInterval::Range(15..=19),
        ]));
        let ipn_range_11 = IpnPattern::Range(Vec::from([
            IpnInterval::Range(0..=10),
            IpnInterval::Range(20..=40),
        ]));

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_1);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x82, 0x00, 0x01]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_1, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_2);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x81, 0x00]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_2, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_3);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x81, 0x07]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_3, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_4);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x86, 0x08, 0x01, 0x00, 0x00, 0x0A, 0x02]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_4, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_5);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x86, 0x01, 0x00, 0x04, 0x02, 0x04, 0x00]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_5, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_6);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x85, 0x01, 0x00, 0x04, 0x02, 0x04]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_6, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_7);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x86, 0x01, 0x00, 0x04, 0x02, 0x04, 0x04]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_7, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_8);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x86, 0x01, 0x01, 0x03, 0x02, 0x04, 0x00]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_8, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_9);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x85, 0x01, 0x01, 0x03, 0x02, 0x04]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_9, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_10);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x86, 0x01, 0x01, 0x03, 0x02, 0x04, 0x04]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_10, parsed_ipn);

        let mut encoder = Encoder::new();
        encoder.emit(&ipn_range_11);
        let serialized = encoder.build();
        assert_eq!(serialized, [0x84, 0x00, 0x0A, 0x08, 0x14]);
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPattern>(&serialized)
            .expect("Failed while parsing IpnRange");
        assert_eq!(ipn_range_11, parsed_ipn);
    }

    #[test]
    fn test_ipn_pattern_roundtrip() {
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(1),
            node_number: IpnPattern::Single(2),
            service_number: IpnPattern::Single(3),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Wildcard,
            node_number: IpnPattern::Single(2),
            service_number: IpnPattern::Single(3),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(1),
            node_number: IpnPattern::Wildcard,
            service_number: IpnPattern::Single(3),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(1),
            node_number: IpnPattern::Single(2),
            service_number: IpnPattern::Wildcard,
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Wildcard,
            node_number: IpnPattern::Wildcard,
            service_number: IpnPattern::Wildcard,
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Range(Vec::from([IpnInterval::Range(1..=5)])),
            node_number: IpnPattern::Single(2),
            service_number: IpnPattern::Single(3),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(1),
            node_number: IpnPattern::Range(Vec::from([IpnInterval::Range(1..=5)])),
            service_number: IpnPattern::Single(3),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(1),
            node_number: IpnPattern::Single(2),
            service_number: IpnPattern::Range(Vec::from([IpnInterval::Range(1..=5)])),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Range(Vec::from([IpnInterval::Range(1..=5)])),
            node_number: IpnPattern::Range(Vec::from([IpnInterval::Range(1..=5)])),
            service_number: IpnPattern::Range(Vec::from([IpnInterval::Range(1..=5)])),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Wildcard,
            node_number: IpnPattern::Range(Vec::from([IpnInterval::Range(1..=5)])),
            service_number: IpnPattern::Single(3),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);

        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(0),
            node_number: IpnPattern::Single(0),
            service_number: IpnPattern::Single(0),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(4294967295),
            node_number: IpnPattern::Single(4294967295),
            service_number: IpnPattern::Single(0),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
        let ipn_item = IpnPatternItem {
            allocator_id: IpnPattern::Single(1),
            node_number: IpnPattern::Single(2),
            service_number: IpnPattern::Single(4294967295),
            // service_number: IpnPattern::Single(18446744073709551615),
        };
        let mut encoder = Encoder::new();
        encoder.emit(&ipn_item);
        let serialized = encoder.build();
        let parsed_ipn = hardy_cbor::decode::parse::<IpnPatternItem>(&serialized)
            .expect("Failed while parsing IpnItem");
        assert_eq!(parsed_ipn, ipn_item);
    }

    #[test]
    fn test_eid_pattern_roundtrip() {
        let eid_pattern: EidPattern = "ipn:[5].[5-9].[5+]".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:[1,5].[1-2,5-9].[1,5-9]".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:[1-2,5].[1,5,9].[1-2,5-9,12+]".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:1.[1,5+].[1-2,5+]".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:[0,2].[0-1,3-4].[0-5,10]".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:[0,2,4,6,8,10,12,14,16,18,20].2.2".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:1.2.3|ipn:4.5.6".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:1.2.3|ipn:4.5.6|ipn:7.8.9".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:1.2.[1-5]|ipn:*.*.*|ipn:7.8.9".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);

        let eid_pattern: EidPattern = "ipn:[1,5-9].[0,3+].[2-4,10,20+]".parse().unwrap();
        let mut encoder = Encoder::new();
        encoder.emit(&eid_pattern);
        let serialized = encoder.build();
        let parsed_pattern = hardy_cbor::decode::parse::<EidPattern>(&serialized)
            .expect("Failed while parsing EidPattern");
        assert_eq!(parsed_pattern, eid_pattern);
    }
}
