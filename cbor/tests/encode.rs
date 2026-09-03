use hardy_cbor::encode::*;
use hex_literal::hex;

// Encodes a single value and returns just the bytes, dropping the
// `ToCbor::Result` payload that byte-only assertions don't care about.
fn enc<T>(value: &T) -> Vec<u8>
where
    T: ToCbor + ?Sized,
{
    emit(value).0
}

// The rfc_* tests below cover RFC 8949, Appendix A:
// https://www.rfc-editor.org/rfc/rfc8949.html#section-appendix.a

#[test]
fn rfc_unsigned_integers() {
    assert_eq!(enc(&0), hex!("00"));
    assert_eq!(enc(&1), hex!("01"));
    assert_eq!(enc(&10), hex!("0a"));
    assert_eq!(enc(&23), hex!("17"));
    assert_eq!(enc(&24), hex!("1818"));
    assert_eq!(enc(&25), hex!("1819"));
    assert_eq!(enc(&100), hex!("1864"));
    assert_eq!(enc(&1000), hex!("1903e8"));
    assert_eq!(enc(&1000000), hex!("1a000f4240"));
    assert_eq!(enc(&1000000000000u64), hex!("1b000000e8d4a51000"));
    assert_eq!(enc(&18446744073709551615u64), hex!("1bffffffffffffffff"));

    /* We do not support BIGNUMs */
    //assert_eq!(enc(18446744073709551616), hex!("c249010000000000000000"));
    //assert_eq!(enc(-18446744073709551616), hex!("3bffffffffffffffff"));
    //assert_eq!(enc(-18446744073709551617), hex!("c349010000000000000000"));
}

#[test]
fn rfc_negative_integers() {
    assert_eq!(enc(&-1), hex!("20"));
    assert_eq!(enc(&-10), hex!("29"));
    assert_eq!(enc(&-100), hex!("3863"));
    assert_eq!(enc(&-1000), hex!("3903e7"));
}

#[test]
fn rfc_floats() {
    assert_eq!(enc(&0.0), hex!("f90000"));
    assert_eq!(enc(&-0.0), hex!("f98000"));
    assert_eq!(enc(&1.0), hex!("f93c00"));
    assert_eq!(enc(&1.1), hex!("fb3ff199999999999a"));
    assert_eq!(enc(&1.5), hex!("f93e00"));
    assert_eq!(enc(&65504.0), hex!("f97bff"));
    assert_eq!(enc(&100000.0), hex!("fa47c35000"));
    assert_eq!(enc(&3.4028234663852886e+38), hex!("fa7f7fffff"));
    assert_eq!(enc(&1.0e+300), hex!("fb7e37e43c8800759c"));
    assert_eq!(enc(&5.960464477539063e-8), hex!("f90001"));
    assert_eq!(enc(&0.00006103515625), hex!("f90400"));
    assert_eq!(enc(&-4.0), hex!("f9c400"));
    assert_eq!(enc(&-4.1), hex!("fbc010666666666666"));
    assert_eq!(enc(&half::f16::INFINITY), hex!("f97c00"));
    assert_eq!(enc(&half::f16::NAN), hex!("f97e00"));
    assert_eq!(enc(&half::f16::NEG_INFINITY), hex!("f9fc00"));
    // RFC 8949 §4.2.2: NaN canonically encodes as the half-precision 0xf97e00,
    // like the infinities in §4.2.1 above (was previously emitted full-width).
    assert_eq!(enc(&f32::NAN), hex!("f97e00"));
    assert_eq!(enc(&f64::NAN), hex!("f97e00"));

    /* According to https://www.rfc-editor.org/rfc/rfc8949.html#section-4.2.1
    +-INF data should go smaller when canonically encoding */
    assert_eq!(
        enc(&f32::INFINITY),
        hex!("f97c00") /*hex!("fa7f800000")*/
    );
    assert_eq!(
        enc(&f32::NEG_INFINITY),
        hex!("f9fc00") /*hex!("faff800000")*/
    );
    assert_eq!(
        enc(&f64::INFINITY),
        hex!("f97c00") /*hex!("fb7ff0000000000000")*/
    );
    assert_eq!(
        enc(&f64::NEG_INFINITY),
        hex!("f9fc00") /*hex!("fbfff0000000000000")*/
    );
}

#[test]
fn rfc_simple_values() {
    assert_eq!(enc(&false), hex!("f4"));
    assert_eq!(enc(&true), hex!("f5"));
    assert_eq!(enc(&None::<i32>), hex!("f7"));
}

#[test]
fn rfc_tags() {
    assert_eq!(
        enc(&Tagged::<0, _>("2013-03-21T20:04:00Z")),
        hex!("c074323031332d30332d32315432303a30343a30305a")
    );
    assert_eq!(enc(&Tagged::<1, _>(&1363896240)), hex!("c11a514b67b0"));
    assert_eq!(
        enc(&Tagged::<1, _>(&1363896240.5)),
        hex!("c1fb41d452d9ec200000")
    );
    assert_eq!(
        emit(&Tagged::<23, _>(&Bytes(&hex!("01020304")))),
        (hex!("d74401020304").into(), 2..6)
    );
    assert_eq!(
        emit(&Tagged::<24, _>(&Bytes(&hex!("6449455446")))),
        (hex!("d818456449455446").into(), 3..8)
    );
    assert_eq!(
        enc(&Tagged::<32, _>("http://www.example.com")),
        hex!("d82076687474703a2f2f7777772e6578616d706c652e636f6d")
    );
}

#[test]
fn rfc_strings() {
    assert_eq!(emit(&Bytes(&[])), (hex!("40").into(), 1..1));
    assert_eq!(
        emit(&Bytes(&hex!("01020304"))),
        (hex!("4401020304").into(), 1..5)
    );
    assert_eq!(enc(""), hex!("60"));
    assert_eq!(enc("a"), hex!("6161"));
    assert_eq!(enc("IETF"), hex!("6449455446"));
    assert_eq!(enc("\"\\"), hex!("62225c"));
    assert_eq!(enc("\u{00fc}"), hex!("62c3bc"));
    assert_eq!(enc("\u{6c34}"), hex!("63e6b0b4"));
    assert_eq!(
        enc("\u{10151}" /* surrogate pair: \u{d800}\u{dd51} */),
        hex!("64f0908591")
    );
}

#[test]
fn rfc_definite_arrays_maps() {
    assert_eq!(*emit_array(Some(0), |_| {}), hex!("80"));
    assert_eq!(enc::<[u16; 0]>(&[]), hex!("80"));
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit(&2);
            a.emit(&3);
        }),
        hex!("83010203")
    );
    assert_eq!(enc(&(1, 2, 3)), hex!("83010203"));
    assert_eq!(enc(&[1, 2, 3]), hex!("83010203"));
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit_array(Some(2), |a| {
                a.emit(&2);
                a.emit(&3);
            });
            a.emit_array(Some(2), |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("8301820203820405")
    );
    assert_eq!(enc(&(1, &(2, 3), &(4, 5))), hex!("8301820203820405"));
    assert_eq!(enc(&(1, &[2, 3], &(4, 5))), hex!("8301820203820405"));
    assert_eq!(enc(&(1, &(2, 3), &[4, 5])), hex!("8301820203820405"));
    assert_eq!(enc(&(1, &[2, 3], &[4, 5])), hex!("8301820203820405"));
    assert_eq!(
        *emit_array(Some(25), |a| {
            for i in 1..=25 {
                a.emit(&i);
            }
        }),
        hex!("98190102030405060708090a0b0c0d0e0f101112131415161718181819")
    );
    assert_eq!(
        enc((1..=25).collect::<Vec<u8>>().as_slice()),
        hex!("98190102030405060708090a0b0c0d0e0f101112131415161718181819")
    );

    assert_eq!(*emit_map(Some(0), |_| {}), hex!("a0"));
    assert_eq!(
        *emit_map(Some(2), |m| {
            m.emit(&1);
            m.emit(&2);
            m.emit(&3);
            m.emit(&4);
        }),
        hex!("a201020304")
    );
    assert_eq!(
        *emit_map(Some(2), |m| {
            m.emit("a");
            m.emit(&1);
            m.emit("b");
            m.emit_array(Some(2), |a| {
                a.emit(&2);
                a.emit(&3);
            });
        }),
        hex!("a26161016162820203")
    );
    assert_eq!(
        *emit_map(Some(2), |m| {
            m.emit("a");
            m.emit(&1);
            m.emit("b");
            m.emit(&(2, 3));
        }),
        hex!("a26161016162820203")
    );
    assert_eq!(
        *emit_map(Some(2), |m| {
            m.emit("a");
            m.emit(&1);
            m.emit("b");
            m.emit(&[2, 3]);
        }),
        hex!("a26161016162820203")
    );
    assert_eq!(
        *emit_array(Some(2), |a| {
            a.emit("a");
            a.emit_map(Some(1), |m| {
                m.emit("b");
                m.emit("c");
            });
        }),
        hex!("826161a161626163")
    );
    assert_eq!(
        *emit_map(Some(5), |m| {
            m.emit("a");
            m.emit("A");
            m.emit("b");
            m.emit("B");
            m.emit("c");
            m.emit("C");
            m.emit("d");
            m.emit("D");
            m.emit("e");
            m.emit("E");
        }),
        hex!("a56161614161626142616361436164614461656145")
    );
}

#[test]
fn rfc_indefinite_items() {
    assert_eq!(
        *emit_byte_stream(|s| {
            s.emit(&hex!("0102"));
            s.emit(&hex!("030405"));
        }),
        hex!("5f42010243030405ff")
    );
    assert_eq!(
        *emit_text_stream(|s| {
            s.emit("strea");
            s.emit("ming");
        }),
        hex!("7f657374726561646d696e67ff")
    );
    assert_eq!(*emit_array(None, |_| {}), hex!("9fff"));
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit_array(Some(2), |a| {
                a.emit(&2);
                a.emit(&3);
            });
            a.emit_array(None, |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("9f018202039f0405ffff")
    );
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit(&(2, 3));
            a.emit_array(None, |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("9f018202039f0405ffff")
    );
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit(&[2, 3]);
            a.emit_array(None, |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("9f018202039f0405ffff")
    );
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit_array(Some(2), |a| {
                a.emit(&2);
                a.emit(&3);
            });
            a.emit_array(Some(2), |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("9f01820203820405ff")
    );
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit(&(2, 3));
            a.emit(&(4, 5));
        }),
        hex!("9f01820203820405ff")
    );
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit(&[2, 3]);
            a.emit(&(4, 5));
        }),
        hex!("9f01820203820405ff")
    );
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit(&(2, 3));
            a.emit(&[4, 5]);
        }),
        hex!("9f01820203820405ff")
    );
    assert_eq!(
        *emit_array(None, |a| {
            a.emit(&1);
            a.emit(&[2, 3]);
            a.emit(&[4, 5]);
        }),
        hex!("9f01820203820405ff")
    );
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit_array(Some(2), |a| {
                a.emit(&2);
                a.emit(&3);
            });
            a.emit_array(None, |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("83018202039f0405ff")
    );
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit(&(2, 3));
            a.emit_array(None, |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("83018202039f0405ff")
    );
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit(&[2, 3]);
            a.emit_array(None, |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("83018202039f0405ff")
    );
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit_array(None, |a| {
                a.emit(&2);
                a.emit(&3);
            });
            a.emit_array(Some(2), |a| {
                a.emit(&4);
                a.emit(&5);
            });
        }),
        hex!("83019f0203ff820405")
    );
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit_array(None, |a| {
                a.emit(&2);
                a.emit(&3);
            });
            a.emit(&(4, 5));
        }),
        hex!("83019f0203ff820405")
    );
    assert_eq!(
        *emit_array(Some(3), |a| {
            a.emit(&1);
            a.emit_array(None, |a| {
                a.emit(&2);
                a.emit(&3);
            });
            a.emit(&[4, 5]);
        }),
        hex!("83019f0203ff820405")
    );
    assert_eq!(
        *emit_array(None, |a| {
            for i in 1..=25 {
                a.emit(&i);
            }
        }),
        hex!("9f0102030405060708090a0b0c0d0e0f101112131415161718181819ff")
    );
    assert_eq!(
        *emit_map(None, |m| {
            m.emit("a");
            m.emit(&1);
            m.emit("b");
            m.emit_array(None, |a| {
                a.emit(&2);
                a.emit(&3);
            });
        }),
        hex!("bf61610161629f0203ffff")
    );
    assert_eq!(
        *emit_array(Some(2), |a| {
            a.emit("a");
            a.emit_map(None, |m| {
                m.emit("b");
                m.emit("c");
            });
        }),
        hex!("826161bf61626163ff")
    );
    assert_eq!(
        *emit_map(None, |m| {
            m.emit("Fun");
            m.emit(&true);
            m.emit("Amt");
            m.emit(&-2);
        }),
        hex!("bf6346756ef563416d7421ff")
    );
}

// A definite-length header is written eagerly from the declared count, so
// a count that disagrees with the emitted items would produce malformed
// CBOR on the wire. The encoder panics instead.
#[test]
#[should_panic(expected = "Too many items added to definite length sequence")]
fn definite_array_overfill() {
    emit_array(Some(1), |a| {
        a.emit(&1);
        a.emit(&2);
    });
}

#[test]
#[should_panic(expected = "Definite length sequence is short of items")]
fn definite_array_underfill() {
    emit_array(Some(2), |a| {
        a.emit(&1);
    });
}

#[test]
#[should_panic(expected = "Too many items added to definite length sequence")]
fn definite_map_overfill() {
    emit_map(Some(1), |m| {
        m.emit(&1);
        m.emit(&2);
        m.emit(&3);
    });
}

#[test]
#[should_panic(expected = "Definite length sequence is short of items")]
fn definite_map_underfill() {
    emit_map(Some(2), |m| {
        m.emit(&1);
        m.emit(&2);
    });
}
