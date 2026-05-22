use crate::{decode::*, *};
use hex_literal::hex;

fn test_simple<T>(expected: T, data: &[u8])
where
    T: FromCbor + PartialEq + core::fmt::Debug,
    T::Error: From<Error> + core::fmt::Debug,
{
    let (v, s, len) = parse::<(T, bool, usize)>(data).unwrap();
    assert!(s);
    assert_eq!(len, data.len());
    assert_eq!(v, expected);
}

fn test_simple_long<T>(expected: T, data: &[u8])
where
    T: FromCbor + PartialEq + core::fmt::Debug,
    T::Error: From<Error> + core::fmt::Debug,
{
    let (v, s, len) = parse::<(T, bool, usize)>(data).unwrap();
    assert!(!s);
    assert_eq!(len, data.len());
    assert_eq!(v, expected);
}

fn test_sub_simple<T, const D: usize>(expected: T, seq: &mut Series<D>)
where
    T: FromCbor + PartialEq + core::fmt::Debug,
    T::Error: From<Error> + core::fmt::Debug,
{
    let (v, s) = seq.parse::<(T, bool)>().unwrap();
    assert!(s);
    assert_eq!(v, expected);
}

fn test_value<F>(data: &[u8], expected_tags: &[u64], f: F)
where
    F: FnOnce(Value),
{
    assert_eq!(
        parse_value(data, |value, shortest, tags| {
            assert!(shortest);
            assert_eq!(tags, expected_tags);
            f(value);
            Ok::<_, Error>(())
        })
        .unwrap()
        .1,
        data.len()
    );
}

fn test_value_long<F>(data: &[u8], expected_tags: &[u64], f: F)
where
    F: FnOnce(Value),
{
    assert_eq!(
        parse_value(data, |value, shortest, tags| {
            assert!(!shortest);
            assert_eq!(tags, expected_tags);
            f(value);
            Ok::<_, Error>(())
        })
        .unwrap()
        .1,
        data.len()
    );
}

fn test_sub_value<F, const D: usize>(expected_tags: &[u64], seq: &mut Series<D>, f: F)
where
    F: FnOnce(Value),
{
    seq.parse_value(|value, shortest, tags| {
        assert!(shortest);
        assert_eq!(tags, expected_tags);
        f(value);
        Ok::<_, Error>(())
    })
    .unwrap();
}

fn test_string(expected: &str, data: &[u8]) {
    test_value(data, &[], |v| {
        assert!(matches!(v, Value::Text(s) if s == expected))
    })
}

fn test_sub_string<const D: usize>(expected: &str, seq: &mut Series<D>) {
    test_sub_value(&[], seq, |v| {
        assert!(matches!(v, Value::Text(s) if s == expected))
    })
}

fn test_array<F>(expected_tags: &[u64], is_definite: bool, data: &[u8], f: F)
where
    F: FnOnce(&mut Array),
{
    test_value(data, expected_tags, |v| match v {
        Value::Array(a) => {
            assert_eq!(is_definite, a.is_definite());
            f(a)
        }
        _ => panic!("Not an array"),
    })
}

fn test_sub_array<F, const D: usize>(
    expected_tags: &[u64],
    is_definite: bool,
    seq: &mut Series<D>,
    f: F,
) where
    F: FnOnce(&mut Array),
{
    test_sub_value(expected_tags, seq, |v| match v {
        Value::Array(a) => {
            assert_eq!(is_definite, a.is_definite());
            f(a)
        }
        _ => panic!("Not an array"),
    })
}

fn test_map<F>(expected_tags: &[u64], is_definite: bool, data: &[u8], f: F)
where
    F: FnOnce(&mut Map),
{
    test_value(data, expected_tags, |v| match v {
        Value::Map(m) => {
            assert_eq!(is_definite, m.is_definite());
            f(m)
        }
        _ => panic!("Not a map"),
    })
}

fn test_sub_map<F, const D: usize>(
    expected_tags: &[u64],
    is_definite: bool,
    seq: &mut Series<D>,
    f: F,
) where
    F: FnOnce(&mut Map),
{
    test_sub_value(expected_tags, seq, |v| match v {
        Value::Map(m) => {
            assert_eq!(is_definite, m.is_definite());
            f(m)
        }
        _ => panic!("Not a map"),
    })
}

#[test]
fn rfc_tests() {
    // RFC 8949, Appendix A:
    // https://www.rfc-editor.org/rfc/rfc8949.html#section-appendix.a

    // LLR 1.1.9: Support all primitive data items (Unsigned Integers)
    test_simple(0, &hex!("00"));
    test_simple(1, &hex!("01"));
    test_simple(10, &hex!("0a"));
    test_simple(23, &hex!("17"));
    test_simple(24, &hex!("1818"));
    test_simple(25, &hex!("1819"));
    test_simple(100, &hex!("1864"));
    test_simple(1000, &hex!("1903e8"));
    test_simple(1000000, &hex!("1a000f4240"));
    test_simple(1000000000000u64, &hex!("1b000000e8d4a51000"));
    test_simple(18446744073709551615u64, &hex!("1bffffffffffffffff"));

    // LLR 1.1.9: Correctly reject unsupported types (Bignums)
    /* We do not support BIGNUMs */
    assert!(parse::<u64>(&hex!("c249010000000000000000")).is_err());
    /*test_simple(
        18446744073709551616,
        &hex!("c249010000000000000000")
    );*/
    assert!(parse::<i64>(&hex!("3bffffffffffffffff")).is_err());
    /*test_simple(
        -18446744073709551616i128,
        &hex!("3bffffffffffffffff")
    );*/
    assert!(parse::<i64>(&hex!("c349010000000000000000")).is_err());
    /*test_simple(
        -18446744073709551617,
        &hex!("c349010000000000000000")
    );*/

    // LLR 1.1.9: Support all primitive data items (Negative Integers)
    test_simple(-1, &hex!("20"));
    test_simple(-10, &hex!("29"));
    test_simple(-100, &hex!("3863"));
    test_simple(-1000, &hex!("3903e7"));
    // LLR 1.1.9: Support all primitive data items (Floating-Point Numbers)
    test_simple(0.0, &hex!("f90000"));
    test_simple(-0.0, &hex!("f98000"));
    test_simple(1.0, &hex!("f93c00"));
    test_simple(1.1, &hex!("fb3ff199999999999a"));
    test_simple(1.5, &hex!("f93e00"));
    test_simple(65504.0, &hex!("f97bff"));
    test_simple(100000.0, &hex!("fa47c35000"));
    test_simple(3.4028234663852886e+38, &hex!("fa7f7fffff"));
    test_simple(1.0e+300, &hex!("fb7e37e43c8800759c"));
    test_simple(5.960464477539063e-8, &hex!("f90001"));
    test_simple(0.00006103515625, &hex!("f90400"));
    test_simple(-4.0, &hex!("f9c400"));
    test_simple(-4.1, &hex!("fbc010666666666666"));
    test_simple(half::f16::INFINITY, &hex!("f97c00"));
    test_value(&hex!("f97e00"), &[], |v| {
        assert!(matches!(v,Value::Float(v) if v.is_nan()))
    });
    test_simple(half::f16::NEG_INFINITY, &hex!("f9fc00"));

    // LLR 1.1.7: Report if a parsed data item is in canonical form (non-canonical floats)
    test_simple_long(f32::INFINITY, &hex!("fa7f800000"));
    test_value_long(&hex!("fa7fc00000"), &[], |v| {
        assert!(matches!(v,Value::Float(v) if v.is_nan()))
    });
    test_simple_long(f32::NEG_INFINITY, &hex!("faff800000"));
    test_simple_long(f64::INFINITY, &hex!("fb7ff0000000000000"));
    test_value_long(&hex!("fb7ff8000000000000"), &[], |v| {
        assert!(matches!(v,Value::Float(v) if v.is_nan()))
    });
    test_simple_long(f64::NEG_INFINITY, &hex!("fbfff0000000000000"));

    // LLR 1.1.9: Support all primitive data items (Simple values and booleans)
    test_simple(false, &hex!("f4"));
    test_simple(true, &hex!("f5"));
    test_value(&hex!("f6"), &[], |v| assert!(matches!(v, Value::Null)));
    test_value(&hex!("f7"), &[], |v| assert!(matches!(v, Value::Undefined)));
    test_value(&hex!("f0"), &[], |v| {
        assert!(matches!(v, Value::Simple(16)))
    });
    test_value(&hex!("f8ff"), &[], |v| {
        assert!(matches!(v, Value::Simple(255)))
    });

    // LLR 1.1.8: Report if a parsed data item has associated tags
    test_value(
        &hex!("c074323031332d30332d32315432303a30343a30305a"),
        &[0],
        |v| assert!(matches!(v, Value::Text("2013-03-21T20:04:00Z"))),
    );
    test_value(&hex!("c11a514b67b0"), &[1], |v| {
        assert!(matches!(v, Value::UnsignedInteger(1363896240)))
    });
    test_value(&hex!("c1fb41d452d9ec200000"), &[1], |v| {
        assert!(matches!(v, Value::Float(1363896240.5)))
    });
    test_value(&hex!("d74401020304"), &[23], |v| {
        assert!(matches!(v, Value::Bytes(v) if v == (2..6)))
    });
    test_value(&hex!("d818456449455446"), &[24], |v| {
        assert!(matches!(v, Value::Bytes(v) if v == (3..8)))
    });
    test_value(
        &hex!("d82076687474703a2f2f7777772e6578616d706c652e636f6d"),
        &[32],
        |v| assert!(matches!(v, Value::Text(v) if v == "http://www.example.com")),
    );

    // LLR 1.1.9: Support all primitive data items (Byte and Text Strings)
    test_value(&hex!("40"), &[], |v| {
        assert!(matches!(v, Value::Bytes(v) if v.is_empty()))
    });
    test_value(&hex!("4401020304"), &[], |v| {
        assert!(matches!(v, Value::Bytes(v) if v == (1..5)))
    });
    test_string("", &hex!("60"));
    test_string("a", &hex!("6161"));
    test_string("IETF", &hex!("6449455446"));
    test_string("\"\\", &hex!("62225c"));
    test_string("\u{00fc}", &hex!("62c3bc"));
    test_string("\u{6c34}", &hex!("63e6b0b4"));
    test_string(
        "\u{10151}", /* surrogate pair: \u{d800}\u{dd51} */
        &hex!("64f0908591"),
    );

    // LLR 1.1.10: Parse items within context of Maps/Arrays correctly (Definite-length Arrays)
    test_array(&[], true, &hex!("80"), |a| assert_eq!(a.count(), Some(0)));
    test_array(&[], true, &hex!("83010203"), |a| {
        test_sub_simple(1, a);
        test_sub_simple(2, a);
        test_sub_simple(3, a);
    });
    test_array(&[], true, &hex!("8301820203820405"), |a| {
        test_sub_simple(1, a);
        test_sub_array(&[], true, a, |a| {
            test_sub_simple(2, a);
            test_sub_simple(3, a);
        });
        test_sub_array(&[], true, a, |a| {
            test_sub_simple(4, a);
            test_sub_simple(5, a);
        });
    });
    test_array(
        &[],
        true,
        &hex!("98190102030405060708090a0b0c0d0e0f101112131415161718181819"),
        |a| {
            for i in 1..=25 {
                test_sub_simple(i, a);
            }
        },
    );

    // LLR 1.1.10: Parse items within context of Maps/Arrays correctly (Definite-length Maps)
    test_map(&[], true, &hex!("a0"), |_| {});
    test_map(&[], true, &hex!("a201020304"), |m| {
        for i in 1..=4 {
            test_sub_simple(i, m);
        }
    });
    test_map(&[], true, &hex!("a26161016162820203"), |m| {
        test_sub_string("a", m);
        test_sub_simple(1, m);
        test_sub_string("b", m);
        test_sub_array(&[], true, m, |a| {
            test_sub_simple(2, a);
            test_sub_simple(3, a);
        });
    });
    test_array(&[], true, &hex!("826161a161626163"), |a| {
        test_sub_string("a", a);
        test_sub_map(&[], true, a, |m| {
            test_sub_string("b", m);
            test_sub_string("c", m);
        });
    });
    test_map(
        &[],
        true,
        &hex!("a56161614161626142616361436164614461656145"),
        |m| {
            for i in ["a", "A", "b", "B", "c", "C", "d", "D", "e", "E"] {
                test_sub_string(i, m);
            }
        },
    );

    // LLR 1.1.5: Handle indefinite length items safely (Indefinite-length Strings)
    {
        let test_data = &hex!("5f42010243030405ff");
        test_value(test_data, &[], |v| match v {
            Value::ByteStream(v) => {
                assert_eq!(
                    hex!("0102030405"),
                    v.into_iter()
                        .fold(Vec::new(), |mut v, b| {
                            v.extend_from_slice(&test_data[b]);
                            v
                        })
                        .as_ref()
                )
            }
            _ => panic!("Expected indefinite byte string"),
        });
    }
    test_value(&hex!("7f657374726561646d696e67ff"), &[], |v| match v {
        Value::TextStream(v) => {
            assert_eq!(
                "streaming",
                v.iter().fold(String::new(), |mut v, b| {
                    v.push_str(b);
                    v
                })
            )
        }
        _ => panic!("Expected indefinite byte string"),
    });

    // LLR 1.1.5: Handle indefinite length items safely (Indefinite-length Arrays)
    test_array(&[], false, &hex!("9fff"), |_| ());
    test_array(&[], false, &hex!("9f018202039f0405ffff"), |a| {
        test_sub_simple(1, a);
        test_sub_array(&[], true, a, |a| {
            test_sub_simple(2, a);
            test_sub_simple(3, a);
        });
        test_sub_array(&[], false, a, |a| {
            test_sub_simple(4, a);
            test_sub_simple(5, a);
        });
    });

    // LLR 1.1.10: Parse items within context of Maps/Arrays correctly (Mixed definite/indefinite arrays)
    test_array(&[], true, &hex!("83018202039f0405ff"), |a| {
        test_sub_simple(1, a);
        test_sub_array(&[], true, a, |a| {
            test_sub_simple(2, a);
            test_sub_simple(3, a);
        });
        test_sub_array(&[], false, a, |a| {
            test_sub_simple(4, a);
            test_sub_simple(5, a);
        });
    });
    test_array(&[], true, &hex!("83019f0203ff820405"), |a| {
        test_sub_simple(1, a);
        test_sub_array(&[], false, a, |a| {
            test_sub_simple(2, a);
            test_sub_simple(3, a);
        });
        test_sub_array(&[], true, a, |a| {
            test_sub_simple(4, a);
            test_sub_simple(5, a);
        });
    });
    test_array(
        &[],
        false,
        &hex!("9f0102030405060708090a0b0c0d0e0f101112131415161718181819ff"),
        |a| {
            for i in 1..=25 {
                test_sub_simple(i, a);
            }
        },
    );

    // LLR 1.1.5: Handle indefinite length items safely (Indefinite-length Maps)
    test_map(&[], false, &hex!("bf61610161629f0203ffff"), |m| {
        test_sub_string("a", m);
        test_sub_simple(1, m);
        test_sub_string("b", m);
        test_sub_array(&[], false, m, |a| {
            test_sub_simple(2, a);
            test_sub_simple(3, a);
        });
    });
    test_array(&[], true, &hex!("826161bf61626163ff"), |a| {
        test_sub_string("a", a);
        test_sub_map(&[], false, a, |m| {
            test_sub_string("b", m);
            test_sub_string("c", m);
        })
    });
    test_map(&[], false, &hex!("bf6346756ef563416d7421ff"), |m| {
        test_sub_string("Fun", m);
        test_sub_simple(true, m);
        test_sub_string("Amt", m);
        test_sub_simple(-2, m);
    });
}

// LLR 1.1.12: Incomplete Item Detection
// Verify that `Error::NeedMoreData` is returned for truncated inputs.
#[test]
fn incomplete_item_detection() {
    // Unsigned integer: major type 0, additional info 24 requires 1 following byte
    assert!(matches!(
        parse::<u64>(&hex!("18")),
        Err(Error::NeedMoreData(1))
    ));

    // Unsigned integer: additional info 25 requires 2 following bytes
    assert!(matches!(
        parse::<u64>(&hex!("19")),
        Err(Error::NeedMoreData(2))
    ));

    // Unsigned integer: additional info 25 with only 1 of 2 bytes
    assert!(matches!(
        parse::<u64>(&hex!("1900")),
        Err(Error::NeedMoreData(1))
    ));

    // Unsigned integer: additional info 26 requires 4 following bytes
    assert!(matches!(
        parse::<u64>(&hex!("1a")),
        Err(Error::NeedMoreData(4))
    ));

    // Unsigned integer: additional info 27 requires 8 following bytes, only 3 given
    assert!(matches!(
        parse::<u64>(&hex!("1b000000")),
        Err(Error::NeedMoreData(5))
    ));

    // Negative integer: same encoding, major type 1
    assert!(matches!(
        parse::<i64>(&hex!("38")),
        Err(Error::NeedMoreData(1))
    ));

    // Byte string: header says 4 bytes, but none follow
    assert!(matches!(
        parse_value(&hex!("44"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::NeedMoreData(4))
    ));

    // Byte string: header says 4 bytes, only 2 follow
    assert!(matches!(
        parse_value(&hex!("440102"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::NeedMoreData(2))
    ));

    // Text string: header says 4 bytes of UTF-8, but none follow
    assert!(matches!(
        parse_value(&hex!("64"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::NeedMoreData(4))
    ));

    // Float16: additional info 25 requires 2 bytes
    assert!(matches!(
        parse_value(&hex!("f9"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::NeedMoreData(2))
    ));

    // Float32: additional info 26 requires 4 bytes, only 1 given
    assert!(matches!(
        parse_value(&hex!("fa00"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::NeedMoreData(3))
    ));

    // Empty input
    assert!(matches!(
        parse_value(&hex!(""), |_, _, _| Ok::<_, Error>(())),
        Err(Error::NeedMoreData(1))
    ));

    // Definite-length array: header says 3 items, but body is truncated
    // NeedMoreData is raised when trying to read the first item
    assert!(matches!(
        parse_array(&hex!("83"), |a, _, _| { a.parse::<u64>() }),
        Err(Error::NeedMoreData(1))
    ));
}

// LLR 1.1.11: Opportunistic Parsing
// Verify `try_parse` returns `Ok(None)` when a sequence is cleanly exhausted.
#[test]
fn opportunistic_parsing() {
    // Definite-length array with 2 items: try_parse returns values then None
    parse_array(&hex!("820102"), |a, _, _| {
        assert_eq!(a.try_parse::<u64>()?, Some(1));
        assert_eq!(a.try_parse::<u64>()?, Some(2));
        assert_eq!(a.try_parse::<u64>()?, None);
        Ok::<_, Error>(())
    })
    .unwrap();

    // Empty definite-length array: try_parse returns None immediately
    parse_array(&hex!("80"), |a, _, _| {
        assert_eq!(a.try_parse::<u64>()?, None);
        Ok::<_, Error>(())
    })
    .unwrap();

    // Indefinite-length array with 1 item: try_parse returns value then None
    parse_array(&hex!("9f01ff"), |a, _, _| {
        assert_eq!(a.try_parse::<u64>()?, Some(1));
        assert_eq!(a.try_parse::<u64>()?, None);
        Ok::<_, Error>(())
    })
    .unwrap();

    // Empty indefinite-length array: try_parse returns None immediately
    parse_array(&hex!("9fff"), |a, _, _| {
        assert_eq!(a.try_parse::<u64>()?, None);
        Ok::<_, Error>(())
    })
    .unwrap();

    // try_parse_value also returns None at end
    parse_array(&hex!("8101"), |a, _, _| {
        assert!(a.try_parse_value(|_, _, _| Ok::<_, Error>(()))?.is_some());
        assert!(a.try_parse_value(|_, _, _| Ok::<_, Error>(()))?.is_none());
        Ok::<_, Error>(())
    })
    .unwrap();

    // Sequence (bare items, no container): try_parse returns values then None
    parse_sequence(&hex!("0102"), |s| {
        assert_eq!(s.try_parse::<u64>()?, Some(1));
        assert_eq!(s.try_parse::<u64>()?, Some(2));
        assert_eq!(s.try_parse::<u64>()?, None);
        Ok::<_, Error>(())
    })
    .unwrap();

    // Contrast: parse (not try_parse) returns NoMoreItems at end
    parse_array(&hex!("8101"), |a, _, _| {
        assert!(a.parse::<u64>().is_ok());
        assert!(matches!(a.parse::<u64>(), Err(Error::NoMoreItems)));
        Ok::<_, Error>(())
    })
    .unwrap();
}

// LLR 1.1.7: Report non-canonical integer encodings
//
// RFC 8949 §4.2.1: integers must use the shortest encoding.
// Values 0-23 fit in the minor value itself (1 byte total).
// Values 24-255 require additional info 24 (2 bytes total).
// Values 256-65535 require additional info 25 (3 bytes total).
// Values 65536-4294967295 require additional info 26 (5 bytes total).
#[test]
fn non_canonical_integers() {
    // Value 0 encoded as 2-byte (additional info 24): non-canonical
    test_simple_long(0u64, &hex!("1800"));

    // Value 23 encoded as 2-byte: non-canonical (fits in minor value)
    test_simple_long(23u64, &hex!("1817"));

    // Value 24 encoded as 2-byte: canonical (doesn't fit in minor)
    test_simple(24u64, &hex!("1818"));

    // Value 0 encoded as 3-byte (additional info 25): non-canonical
    test_simple_long(0u64, &hex!("190000"));

    // Value 255 encoded as 3-byte: non-canonical (fits in 2-byte)
    test_simple_long(255u64, &hex!("1900ff"));

    // Value 256 encoded as 3-byte: canonical
    test_simple(256u64, &hex!("190100"));

    // Value 0 encoded as 5-byte (additional info 26): non-canonical
    test_simple_long(0u64, &hex!("1a00000000"));

    // Value 65535 encoded as 5-byte: non-canonical (fits in 3-byte)
    test_simple_long(65535u64, &hex!("1a0000ffff"));

    // Value 65536 encoded as 5-byte: canonical
    test_simple(65536u64, &hex!("1a00010000"));

    // Value 0 encoded as 9-byte (additional info 27): non-canonical
    test_simple_long(0u64, &hex!("1b0000000000000000"));

    // Value 4294967295 encoded as 9-byte: non-canonical (fits in 5-byte)
    test_simple_long(4294967295u64, &hex!("1b00000000ffffffff"));

    // Value 4294967296 encoded as 9-byte: canonical
    test_simple(4294967296u64, &hex!("1b0000000100000000"));

    // Negative integers: same encoding rules, major type 1
    // Value -1 (minor value 0) encoded as 2-byte: non-canonical
    test_simple_long(-1i64, &hex!("3800"));

    // Value -25 (minor value 24) encoded as 2-byte: canonical
    test_simple(-25i64, &hex!("3818"));

    // Value -24 (minor value 23) encoded as 2-byte: non-canonical
    test_simple_long(-24i64, &hex!("3817"));
}

// Error path tests for malformed CBOR (beyond truncation)
#[test]
fn malformed_cbor() {
    // Invalid minor values (28, 29, 30) are reserved and must be rejected
    assert!(matches!(
        parse::<u64>(&hex!("1c")),
        Err(Error::InvalidMinorValue(28))
    ));
    assert!(matches!(
        parse::<u64>(&hex!("1d")),
        Err(Error::InvalidMinorValue(29))
    ));
    assert!(matches!(
        parse::<u64>(&hex!("1e")),
        Err(Error::InvalidMinorValue(30))
    ));

    // Reserved minor values in other major types (negative int, bytes, text)
    assert!(matches!(
        parse::<i64>(&hex!("3c")),
        Err(Error::InvalidMinorValue(28))
    ));
    assert!(matches!(
        parse_value(&hex!("5c"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidMinorValue(28))
    ));
    assert!(matches!(
        parse_value(&hex!("7c"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidMinorValue(28))
    ));

    // Invalid simple values: 2-byte simple type with value < 32 is not allowed
    // (values 0-31 must use the 1-byte encoding directly in the minor value)
    assert!(matches!(
        parse_value(&hex!("f800"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidSimpleType(0))
    ));
    assert!(matches!(
        parse_value(&hex!("f81f"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidSimpleType(31))
    ));
    // Value 32 in 2-byte form is valid
    parse_value(&hex!("f820"), |v, _, _| {
        assert!(matches!(v, Value::Simple(32)));
        Ok::<_, Error>(())
    })
    .unwrap();

    // Invalid UTF-8 in text string
    assert!(matches!(
        parse_value(&hex!("62ff80"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidUtf8(_))
    ));

    // Indefinite-length byte string with wrong chunk type (text instead of bytes)
    assert!(matches!(
        parse_value(&hex!("5f6161ff"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidChunk)
    ));

    // Indefinite-length text string with wrong chunk type (bytes instead of text)
    assert!(matches!(
        parse_value(&hex!("7f4101ff"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidChunk)
    ));

    // Type mismatch: expecting u64, got text string
    assert!(matches!(
        parse::<u64>(&hex!("6161")),
        Err(Error::IncorrectType(_, _))
    ));

    // Type mismatch: expecting u64, got array
    assert!(matches!(
        parse::<u64>(&hex!("80")),
        Err(Error::IncorrectType(_, _))
    ));

    // AdditionalItems: array has 1 item but trying to parse 2
    assert!(matches!(
        parse_array(&hex!("8101"), |a, _, _| {
            a.parse::<u64>()?;
            a.parse::<u64>()?;
            Ok::<_, Error>(())
        }),
        Err(Error::NoMoreItems)
    ));

    // Reserved minor values in major type 7 (simple/float)
    // Minor values 28-30 are unassigned for major type 7
    assert!(matches!(
        parse_value(&hex!("fc"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidSimpleType(28))
    ));
    assert!(matches!(
        parse_value(&hex!("fd"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidSimpleType(29))
    ));
    assert!(matches!(
        parse_value(&hex!("fe"), |_, _, _| Ok::<_, Error>(())),
        Err(Error::InvalidSimpleType(30))
    ));

    // Unterminated indefinite-length array: data ends before break code 0xFF
    // 9f 01 02 = indefinite array with items 1, 2 but no break
    assert!(matches!(
        parse_value(&hex!("9f0102"), |v, _, _| {
            match v {
                Value::Array(a) => {
                    a.try_parse::<u64>()?; // 1
                    a.try_parse::<u64>()?; // 2
                    a.try_parse::<u64>()?; // hits end of data
                    Ok(())
                }
                _ => panic!("expected array"),
            }
        }),
        Err(Error::NeedMoreData(_))
    ));

    // Unterminated indefinite-length map
    assert!(matches!(
        parse_value(&hex!("bf6161"), |v, _, _| {
            match v {
                Value::Map(m) => {
                    m.try_parse_value(|_, _, _| Ok::<_, Error>(()))?; // key "a"
                    m.try_parse_value(|_, _, _| Ok::<_, Error>(()))?; // hits end of data
                    Ok(())
                }
                _ => panic!("expected map"),
            }
        }),
        Err(Error::NeedMoreData(_))
    ));

    // PartialMap: indefinite-length map with a key but no value
    // bf 61 61 ff = indefinite map { "a": <break> } — key "a" has no value
    assert!(matches!(
        parse_value(&hex!("bf6161ff"), |v, _, _| {
            match v {
                Value::Map(m) => {
                    // Try to read key — should succeed
                    m.try_parse_value(|_, _, _| Ok::<_, Error>(()))?;
                    // Trying to read value hits break code with odd item count
                    m.try_parse_value(|_, _, _| Ok::<_, Error>(()))?;
                    Ok(())
                }
                _ => panic!("expected map"),
            }
        }),
        Err(Error::PartialMap)
    ));

    // MaxRecursion: deeply nested arrays exceeding recursion limit
    // Build a nested array 300 levels deep: [[[[...]]]]
    // Each level is 0x81 (definite array of 1 item) with 0x00 at the centre
    let depth = 300usize;
    let mut nested = alloc::vec![0x81u8; depth];
    nested.push(0x00); // innermost value: unsigned 0
    assert!(matches!(
        skip_value(&nested, 16), // limit recursion to 16 levels
        Err(Error::MaxRecursion)
    ));

    // Verify skip succeeds when within recursion limit
    let shallow_depth = 5usize;
    let mut shallow = alloc::vec![0x81u8; shallow_depth];
    shallow.push(0x00);
    skip_value(&shallow, 16).unwrap(); // 5 levels, limit 16 — should succeed
}

#[test]
fn head_consumed_bytes() {
    // Head::from_cbor returns the bytes consumed by the marker head
    // only — for arrays, maps, and indefinite-length strings the contained
    // items remain in the buffer. For definite-length strings the payload
    // is NOT included (just the head + length prefix).

    // Unsigned integer: 0x18 0x64 = uint 100, consumes 2 bytes
    let data = hex!("18 64");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::UnsignedInteger(100)));
    assert_eq!(len, 2);

    // Negative integer: 0x20 = -1, consumes 1 byte
    let data = hex!("20");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::NegativeInteger(0)));
    assert_eq!(len, 1);

    // Definite byte string: 0x43 + 3 bytes payload — head is 1 byte,
    // payload length is encoded inline, consumed = 1 (head) only
    let data = hex!("43 01 02 03");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Bytes(Some(3))));
    assert_eq!(len, 1);

    // Definite text string: 0x65 "hello" — head is 1 byte
    let data = hex!("65 68 65 6c 6c 6f");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Text(Some(5))));
    assert_eq!(len, 1);

    // Definite byte string with 2-byte length: 0x58 0x03 + 3 bytes
    // Head = 1 byte marker + 1 byte length = 2 bytes consumed
    let data = hex!("58 03 01 02 03");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Bytes(Some(3))));
    assert_eq!(len, 2);

    // Indefinite byte string: 0x5F — just the head byte
    let data = hex!("5F 43 01 02 03 FF");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Bytes(None)));
    assert_eq!(len, 1);

    // Indefinite text string: 0x7F — just the head byte
    let data = hex!("7F 63 66 6f 6f FF");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Text(None)));
    assert_eq!(len, 1);

    // Definite array of 3: 0x83 — head only, elements not consumed
    let data = hex!("83 01 02 03");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Array(Some(3))));
    assert_eq!(len, 1);

    // Indefinite array: 0x9F — head only
    let data = hex!("9F 01 02 FF");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Array(None)));
    assert_eq!(len, 1);

    // Definite map of 1 pair: 0xA1 — head only
    let data = hex!("A1 01 02");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Map(Some(1))));
    assert_eq!(len, 1);

    // Indefinite map: 0xBF — head only
    let data = hex!("BF 01 02 FF");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Map(None)));
    assert_eq!(len, 1);

    // Tagged uint: tag 1 + uint 0 — tag head (2 bytes) + value head (1 byte) = 3
    let data = hex!("C1 00");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::UnsignedInteger(0)));
    assert_eq!(m.tags.as_slice(), &[1u64]);
    assert_eq!(len, 2);

    // Nested tags: tag 1, tag 2 + uint 0 = 3 bytes of tags + 1 byte value = 4
    let data = hex!("C1 C2 00");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::UnsignedInteger(0)));
    assert_eq!(m.tags.as_slice(), &[1u64, 2]);
    assert_eq!(len, 3);

    // False: 0xF4 — 1 byte
    let data = hex!("F4");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::False));
    assert_eq!(len, 1);

    // True: 0xF5 — 1 byte
    let data = hex!("F5");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::True));
    assert_eq!(len, 1);

    // Null: 0xF6 — 1 byte
    let data = hex!("F6");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Null));
    assert_eq!(len, 1);

    // Undefined: 0xF7 — 1 byte
    let data = hex!("F7");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Undefined));
    assert_eq!(len, 1);

    // Float16: 0xF9 + 2 bytes = 3 bytes
    let data = hex!("F9 3C 00");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Float(_)));
    assert_eq!(len, 3);

    // Float32: 0xFA + 4 bytes = 5 bytes
    let data = hex!("FA 47 C3 50 00");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Float(_)));
    assert_eq!(len, 5);

    // Float64: 0xFB + 8 bytes = 9 bytes
    let data = hex!("FB 7E 37 E4 3C 88 00 75 9C");
    let (m, _, len) = Head::from_cbor(&data).unwrap();
    assert!(matches!(m.marker, Marker::Float(_)));
    assert_eq!(len, 9);

    // Break stop code: 0xFF is a control byte, not a data item, so
    // Head::from_cbor rejects it. Callers that need to detect it as an
    // indefinite-length terminator do so by direct byte comparison.
    let data = hex!("FF");
    assert!(matches!(
        Head::from_cbor(&data),
        Err(Error::InvalidSimpleType(31))
    ));

    // Tagged 0xFF is also rejected — a tagged control byte is malformed.
    let data = hex!("C0 FF");
    assert!(matches!(
        Head::from_cbor(&data),
        Err(Error::InvalidSimpleType(31))
    ));
}

// Regression: skip_value on a definite-length map must skip 2*N items,
// not N. Marker::Map(Some(n)) carries the pair count, but skip_value
// walks raw items and needs the doubled count.
#[test]
fn skip_value_definite_map_pair_count() {
    // Map { 1: 2, 3: 4 } — 5 bytes
    let map = hex!("a2 01 02 03 04");
    let (shortest, len) = skip_value(&map, 16).unwrap();
    assert!(shortest);
    assert_eq!(len, map.len());

    // Map followed by sentinel uint 66 — skip must consume only the map.
    let data = hex!("a2 01 02 03 04 18 42");
    let (_, len) = skip_value(&data, 16).unwrap();
    assert_eq!(len, 5);

    // And via Series::skip_value inside a sequence.
    parse_sequence(&data, |s| {
        s.skip_value(16)?;
        let v: u64 = s.parse()?;
        assert_eq!(v, 66);
        Ok::<_, Error>(())
    })
    .unwrap();

    // Map nested inside an array — outer skip must clear the whole map.
    // [ {1:2, 3:4}, 66 ]
    let data = hex!("82 a2 01 02 03 04 18 42");
    parse_array(&data, |a, _, _| {
        a.skip_value(16)?;
        let v: u64 = a.parse()?;
        assert_eq!(v, 66);
        Ok::<_, Error>(())
    })
    .unwrap();
}

// Regression: skip_to_end on a D=0 Sequence must terminate.
// The old `for _ in 0..D` ran zero times for D=0 and left offset
// unchanged, infinite-looping.
#[test]
fn skip_to_end_sequence() {
    parse_sequence(&hex!("01 02 03"), |s| {
        s.skip_to_end(16)?;
        Ok::<_, Error>(())
    })
    .unwrap();

    // Empty sequence terminates immediately.
    parse_sequence(&hex!(""), |s| {
        s.skip_to_end(16)?;
        Ok::<_, Error>(())
    })
    .unwrap();
}

// Series::Debug on a partially-consumed array prepends `...` so the reader
// can see the rendering doesn't start at item zero.
#[test]
fn debug_array_mid_drain() {
    // [1, 2, 3]
    parse_array(&hex!("83 01 02 03"), |a, _, _| {
        let _: u64 = a.parse()?; // consume the 1
        let s = format!("{a:?}");
        // Items 2 and 3 remain; leading `...` signals prior consumption.
        assert!(s.contains("..."), "expected leading ..., got {s}");
        assert!(
            s.contains('2') && s.contains('3'),
            "expected 2 and 3, got {s}"
        );
        // Drain the rest so parse_array's complete() doesn't return AdditionalItems.
        a.skip_to_end(16)?;
        Ok::<_, Error>(())
    })
    .unwrap();
}

// Series::Debug on a map between key and value (odd `parsed`) renders the
// dangling value paired with `...`, keeping subsequent pairs aligned.
// Without this, the value would be misread as a key and every following
// pair would be shifted by one item.
#[test]
fn debug_map_mid_pair() {
    // {1: 2, 3: 4} — definite-length, 2 pairs.
    parse_map(&hex!("A2 01 02 03 04"), |m, _, _| {
        let _: u64 = m.parse()?; // consume key 1; value 2 still pending
        let s = format!("{m:?}");
        // Expect `...: 2` for the dangling value and `3: 4` for the remaining pair.
        assert!(s.contains("..."), "expected ... placeholder, got {s}");
        assert!(s.contains('2'), "expected dangling value 2, got {s}");
        assert!(
            s.contains('3') && s.contains('4'),
            "expected pair 3:4, got {s}"
        );
        // Drain the rest so parse_map's complete() doesn't return AdditionalItems.
        m.skip_to_end(16)?;
        Ok::<_, Error>(())
    })
    .unwrap();
}

// Series::Debug on a map after a full pair was consumed (even `parsed > 0`)
// leads with a `...: ...` placeholder pair so the rendering doesn't
// silently look like the whole map.
#[test]
fn debug_map_mid_pairs_even() {
    // {1: 2, 3: 4}
    parse_map(&hex!("A2 01 02 03 04"), |m, _, _| {
        let _: u64 = m.parse()?; // key 1
        let _: u64 = m.parse()?; // value 2 — parsed = 2 (even, > 0)
        let s = format!("{m:?}");
        assert!(s.contains("..."), "expected placeholder pair, got {s}");
        assert!(
            s.contains('3') && s.contains('4'),
            "expected pair 3:4, got {s}"
        );
        // Drain the rest so parse_map's complete() doesn't return AdditionalItems.
        m.skip_to_end(16)?;
        Ok::<_, Error>(())
    })
    .unwrap();
}

// Regression: Series::count() must not panic when called on a fully-consumed
// Sequence (D=0). `at_end` sets `self.count = Some(self.parsed)` once the
// input is drained, and `count() = self.count.map(|c| c / D)` previously
// divided by zero in that state.
#[test]
fn sequence_count_after_drain() {
    parse_sequence(&hex!("01 02 03"), |s| {
        while !s.at_end()? {
            let _: u64 = s.parse()?;
        }
        // Drained, at_end true, count now Some(parsed) — must not panic.
        assert_eq!(s.count(), Some(3));
        Ok::<_, Error>(())
    })
    .unwrap();
}

// Regression: skip_value must bounds-check definite-length string payloads
// against the input buffer. A malformed item claiming more bytes than the
// buffer holds previously returned an out-of-range offset that the next
// parse step misreported as NeedMoreData(1) (or similar) — losing the
// actual shortfall and obscuring the error site. The bounds check now
// surfaces the correct NeedMoreData at the right point.
#[test]
fn skip_value_truncated_definite_strings() {
    // bytes(4) header but only 2 payload bytes follow — short by 2.
    assert!(matches!(
        skip_value(&hex!("44 DE AD"), 16),
        Err(Error::NeedMoreData(2))
    ));

    // text(4) header but only 2 payload bytes follow — short by 2.
    assert!(matches!(
        skip_value(&hex!("64 41 42"), 16),
        Err(Error::NeedMoreData(2))
    ));

    // bytes(8-byte length) claiming a huge payload against a tiny buffer.
    // header is 1 marker + 8 length bytes = 9 bytes; payload claimed is
    // 1_000_000 bytes; buffer is 9 bytes — short by 1_000_000.
    assert!(matches!(
        skip_value(&hex!("5B 00 00 00 00 00 0F 42 40"), 16),
        Err(Error::NeedMoreData(1_000_000))
    ));

    // Same regression inside an indefinite-length byte string: a chunk
    // claiming more bytes than the buffer holds.
    // 5F (bytes indef) 44 (bytes(4)) DE AD — chunk claims 4, only 2 follow.
    assert!(matches!(
        skip_value(&hex!("5F 44 DE AD"), 16),
        Err(Error::NeedMoreData(2))
    ));

    // Same for indefinite-length text string.
    // 7F (text indef) 64 (text(4)) 41 42 — chunk claims 4, only 2 follow.
    assert!(matches!(
        skip_value(&hex!("7F 64 41 42"), 16),
        Err(Error::NeedMoreData(2))
    ));
}

// Regression: skip_value on an indefinite-length map with an odd number
// of items before the break must return Error::PartialMap, matching the
// behaviour of the Series-driven path.
#[test]
fn skip_value_indefinite_map_partial() {
    // BF 01 02 03 FF — indefinite map { 1:2, 3:<break> } — three items.
    assert!(matches!(
        skip_value(&hex!("BF 01 02 03 FF"), 16),
        Err(Error::PartialMap)
    ));

    // Even count succeeds.
    let (shortest, len) = skip_value(&hex!("BF 01 02 03 04 FF"), 16).unwrap();
    assert!(shortest);
    assert_eq!(len, 6);

    // Empty indefinite map succeeds.
    let (shortest, len) = skip_value(&hex!("BF FF"), 16).unwrap();
    assert!(shortest);
    assert_eq!(len, 2);
}
