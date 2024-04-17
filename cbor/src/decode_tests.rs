use super::decode::*;
use hex_literal::hex;

#[test]
fn rfc_tests() {
    // RFC 8949, Appendix A:
    // https://www.rfc-editor.org/rfc/rfc8949.html#section-appendix.a

    assert_eq!(0, parse(&hex!("00")).unwrap());
    assert_eq!(1, parse(&hex!("01")).unwrap());
    assert_eq!(10, parse(&hex!("0a")).unwrap());
    assert_eq!(23, parse(&hex!("17")).unwrap());
    assert_eq!(24, parse(&hex!("1818")).unwrap());
    assert_eq!(25, parse(&hex!("1819")).unwrap());
    assert_eq!(100, parse(&hex!("1864")).unwrap());
    assert_eq!(1000, parse(&hex!("1903e8")).unwrap());
    assert_eq!(1000000, parse(&hex!("1a000f4240")).unwrap());
    assert_eq!(
        1000000000000u64,
        parse(&hex!("1b000000e8d4a51000")).unwrap()
    );
    assert_eq!(
        18446744073709551615u64,
        parse(&hex!("1bffffffffffffffff")).unwrap()
    );
    assert!(parse::<u64>(&hex!("c249010000000000000000")).is_err());
    /*assert_eq!(
        18446744073709551616,
        parse(&hex!("c249010000000000000000")).unwrap()
    );*/
    assert!(parse::<i64>(&hex!("3bffffffffffffffff")).is_err());
    /*assert_eq!(
        -18446744073709551616i128,
        parse(&hex!("3bffffffffffffffff")).unwrap()
    );*/
    assert!(parse::<i64>(&hex!("c349010000000000000000")).is_err());
    /*assert_eq!(
        -18446744073709551617,
        parse(&hex!("c349010000000000000000")).unwrap()
    );*/
    assert_eq!(-1, parse(&hex!("20")).unwrap());
    assert_eq!(-10, parse(&hex!("29")).unwrap());
    assert_eq!(-100, parse(&hex!("3863")).unwrap());
    assert_eq!(-1000, parse(&hex!("3903e7")).unwrap());
    assert_eq!(0.0, parse(&hex!("f90000")).unwrap());
    assert_eq!(-0.0, parse(&hex!("f98000")).unwrap());
    assert_eq!(1.0, parse(&hex!("f93c00")).unwrap());
    assert_eq!(1.1, parse(&hex!("fb3ff199999999999a")).unwrap());
    assert_eq!(1.5, parse(&hex!("f93e00")).unwrap());
    assert_eq!(65504.0, parse(&hex!("f97bff")).unwrap());
    assert_eq!(100000.0, parse(&hex!("fa47c35000")).unwrap());
    assert_eq!(3.4028234663852886e+38, parse(&hex!("fa7f7fffff")).unwrap());
    assert_eq!(1.0e+300, parse(&hex!("fb7e37e43c8800759c")).unwrap());
    assert_eq!(5.960464477539063e-8, parse(&hex!("f90001")).unwrap());
    assert_eq!(0.00006103515625, parse(&hex!("f90400")).unwrap());
    assert_eq!(-4.0, parse(&hex!("f9c400")).unwrap());
    assert_eq!(-4.1, parse(&hex!("fbc010666666666666")).unwrap());
    assert_eq!(f32::INFINITY, parse(&hex!("f97c00")).unwrap());
    assert!(parse::<f32>(&hex!("f97e00")).unwrap().is_nan());
    assert_eq!(f32::NEG_INFINITY, parse(&hex!("f9fc00")).unwrap());
    assert_eq!(f64::INFINITY, parse(&hex!("fa7f800000")).unwrap());
    assert!(parse::<f32>(&hex!("fa7fc00000")).unwrap().is_nan());
    assert_eq!(f64::NEG_INFINITY, parse(&hex!("faff800000")).unwrap());
    assert_eq!(f64::INFINITY, parse(&hex!("fb7ff0000000000000")).unwrap());
    assert!(parse::<f64>(&hex!("fb7ff8000000000000")).unwrap().is_nan());
    assert_eq!(
        f64::NEG_INFINITY,
        parse(&hex!("fbfff0000000000000")).unwrap()
    );
    assert_eq!(false, parse(&hex!("f4")).unwrap());
    assert_eq!(true, parse(&hex!("f5")).unwrap());
    assert!(
        parse_value(&hex!("f6"), |value, tags| {
            assert!(tags.is_empty());
            match value {
                Value::Null => Ok(true),
                _ => Ok(false),
            }
        })
        .unwrap()
        .0
    );
    assert!(
        parse_value(&hex!("f7"), |value, tags| {
            assert!(tags.is_empty());
            match value {
                Value::Undefined => Ok(true),
                _ => Ok(false),
            }
        })
        .unwrap()
        .0
    );
    assert!(
        parse_value(&hex!("f0"), |value, tags| {
            assert!(tags.is_empty());
            match value {
                Value::Simple(16) => Ok(true),
                _ => Ok(false),
            }
        })
        .unwrap()
        .0
    );
    assert_eq!(
        (true, 2),
        parse_value(&hex!("f8ff"), |value, tags| {
            assert!(tags.is_empty());
            match value {
                Value::Simple(255) => Ok(true),
                _ => Ok(false),
            }
        })
        .unwrap()
    );
    assert_eq!(
        (true, 22),
        parse_value(
            &hex!("c074323031332d30332d32315432303a30343a30305a"),
            |value, tags| match value {
                Value::Text("2013-03-21T20:04:00Z", false) if tags == vec![0] => Ok(true),
                _ => Ok(false),
            }
        )
        .unwrap()
    );
    assert_eq!(
        (1363896240, 6, vec![1]),
        parse_detail(&hex!("c11a514b67b0")).unwrap()
    );
    assert_eq!(
        (1363896240.5, 10, vec![1]),
        parse_detail(&hex!("c1fb41d452d9ec200000")).unwrap()
    );
    assert_eq!(
        (true, 6),
        parse_value(&hex!("d74401020304"), |value, tags| match value {
            Value::Bytes(v, false) if v == hex!("01020304") && tags == vec![23] => Ok(true),
            _ => Ok(false),
        })
        .unwrap()
    );
    assert_eq!(
        (true, 8),
        parse_value(&hex!("d818456449455446"), |value, tags| match value {
            Value::Bytes(v, false) if v == hex!("6449455446") && tags == vec![24] => Ok(true),
            _ => Ok(false),
        })
        .unwrap()
    );
    assert_eq!(
        (true, 25),
        parse_value(
            &hex!("d82076687474703a2f2f7777772e6578616d706c652e636f6d"),
            |value, tags| match value {
                Value::Text(v, false) if v == "http://www.example.com" && tags == vec![32] =>
                    Ok(true),
                _ => Ok(false),
            }
        )
        .unwrap()
    );
    assert!(parse::<Vec<u8>>(&hex!("40")).unwrap().is_empty());
    assert_eq!(
        hex!("01020304").to_vec(),
        parse::<Vec<u8>>(&hex!("4401020304")).unwrap()
    );
    assert!(parse::<String>(&hex!("60")).unwrap().is_empty());
    assert_eq!("a", &parse::<String>(&hex!("6161")).unwrap());
    assert_eq!("IETF", &parse::<String>(&hex!("6449455446")).unwrap());
    assert_eq!("\"\\", &parse::<String>(&hex!("62225c")).unwrap());
    assert_eq!("\u{00fc}", &parse::<String>(&hex!("62c3bc")).unwrap());
    assert_eq!("\u{6c34}", &parse::<String>(&hex!("63e6b0b4")).unwrap());
    assert_eq!(
        "\u{10151}", /* surrogate pair: \u{d800}\u{dd51} */
        &parse::<String>(&hex!("64f0908591")).unwrap()
    );
    assert_eq!(
        (0, 1),
        parse_array(&hex!("80"), |a, tags| {
            assert!(tags.is_empty());
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (vec![1, 2, 3], 4),
        parse_array(&hex!("83010203"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_some());
            let v = vec![a.parse()?, a.parse()?, a.parse()?];
            a.end_or_else(|| Error::UnparsedItems.into())?;
            Ok(v)
        })
        .unwrap()
    );
    assert_eq!(
        (3, 8),
        parse_array(&hex!("8301820203820405"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_some());
            assert_eq!(1, a.parse::<usize>()?);
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(2, a.parse::<usize>()?);
                assert_eq!(3, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(4, a.parse::<usize>()?);
                assert_eq!(5, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25
            ],
            29
        ),
        parse_array(
            &hex!("98190102030405060708090a0b0c0d0e0f101112131415161718181819"),
            |a, tags| {
                assert!(tags.is_empty());
                let mut v = Vec::new();
                while let Some(value) = a.try_parse()? {
                    v.push(value);
                }
                Ok(v)
            }
        )
        .unwrap()
    );
    assert_eq!(
        (0, 1),
        parse_map(&hex!("a0"), |m, tags| {
            assert!(tags.is_empty());
            m.end_or_else(|| Error::UnparsedItems.into())?;
            Ok(m.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (vec![1, 2, 3, 4], 5),
        parse_map(&hex!("a201020304"), |m, tags| {
            assert!(tags.is_empty());
            assert!(m.count().is_some());
            let v = vec![m.parse()?, m.parse()?, m.parse()?, m.parse()?];
            m.end_or_else(|| Error::UnparsedItems.into())?;
            Ok(v)
        })
        .unwrap()
    );
    assert_eq!(
        (2, 9),
        parse_map(&hex!("a26161016162820203"), |m, tags| {
            assert!(tags.is_empty());
            assert!(m.count().is_some());
            assert_eq!("a".to_string(), m.parse::<String>()?);
            assert_eq!(1, m.parse::<usize>()?);
            assert_eq!("b".to_string(), m.parse::<String>()?);
            m.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(2, a.parse::<usize>()?);
                assert_eq!(3, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            Ok(m.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (2, 8),
        parse_array(&hex!("826161a161626163"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_some());
            assert_eq!("a".to_string(), a.parse::<String>()?);
            a.parse_map(|m, _, tags| {
                assert!(tags.is_empty());
                assert!(m.count().is_some());
                assert_eq!("b".to_string(), m.parse::<String>()?);
                assert_eq!("c".to_string(), m.parse::<String>()?);
                m.end_or_else(|| Error::UnparsedItems.into())
            })?;
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (
            vec!["a", "A", "b", "B", "c", "C", "d", "D", "e", "E"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            21
        ),
        parse_map(
            &hex!("a56161614161626142616361436164614461656145"),
            |m, tags| {
                assert!(tags.is_empty());
                let mut v = Vec::new();
                while let Some(value) = m.try_parse()? {
                    v.push(value);
                    v.push(m.parse()?);
                }
                Ok(v)
            }
        )
        .unwrap()
    );
    assert_eq!(
        (true, 9),
        parse_value(&hex!("5f42010243030405ff"), |value, tags| {
            assert!(tags.is_empty());
            match value {
                Value::Bytes(v, true) if v == hex!("0102030405") => Ok(true),
                _ => Ok(false),
            }
        })
        .unwrap()
    );
    assert_eq!(
        (true, 13),
        parse_value(&hex!("7f657374726561646d696e67ff"), |value, tags| {
            assert!(tags.is_empty());
            match value {
                Value::Text(v, true) if v == "streaming" => Ok(true),
                _ => Ok(false),
            }
        })
        .unwrap()
    );
    assert_eq!(
        (0, 2),
        parse_array(&hex!("9fff"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_none());
            a.end_or_else(|| Error::UnparsedItems.into())?;
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (3, 10),
        parse_array(&hex!("9f018202039f0405ffff"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_none());
            assert_eq!(1, a.parse::<usize>()?);
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(2, a.parse::<usize>()?);
                assert_eq!(3, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_none());
                assert_eq!(4, a.parse::<usize>()?);
                assert_eq!(5, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            a.end_or_else(|| Error::UnparsedItems.into())?;
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (3, 9),
        parse_array(&hex!("9f01820203820405ff"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_none());
            assert_eq!(1, a.parse::<usize>()?);
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(2, a.parse::<usize>()?);
                assert_eq!(3, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(4, a.parse::<usize>()?);
                assert_eq!(5, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            a.end_or_else(|| Error::UnparsedItems.into())?;
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (3, 9),
        parse_array(&hex!("83018202039f0405ff"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_some());
            assert_eq!(1, a.parse::<usize>()?);
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(2, a.parse::<usize>()?);
                assert_eq!(3, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_none());
                assert_eq!(4, a.parse::<usize>()?);
                assert_eq!(5, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (3, 9),
        parse_array(&hex!("83019f0203ff820405"), |a, tags| {
            assert!(tags.is_empty());
            assert!(a.count().is_some());
            assert_eq!(1, a.parse::<usize>()?);
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_none());
                assert_eq!(2, a.parse::<usize>()?);
                assert_eq!(3, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            a.parse_array(|a, _, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_some());
                assert_eq!(4, a.parse::<usize>()?);
                assert_eq!(5, a.parse::<usize>()?);
                a.end_or_else(|| Error::UnparsedItems.into())
            })?;
            Ok(a.count().unwrap())
        })
        .unwrap()
    );
    assert_eq!(
        (
            vec![
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25
            ],
            29
        ),
        parse_array(
            &hex!("9f0102030405060708090a0b0c0d0e0f101112131415161718181819ff"),
            |a, tags| {
                assert!(tags.is_empty());
                assert!(a.count().is_none());
                let mut v = Vec::new();
                while let Some(value) = a.try_parse()? {
                    v.push(value);
                }
                Ok(v)
            }
        )
        .unwrap()
    );
    /* {_ "a": 1, "b": [_ 2, 3]}	0xbf61610161629f0203ffff
    ["a", {_ "b": "c"}]	0x826161bf61626163ff
    {_ "Fun": true, "Amt": -2}	0xbf6346756ef563416d7421ff
        */
}
