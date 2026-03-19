use super::{decode::*, *};
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
