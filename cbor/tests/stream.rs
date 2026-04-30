use hardy_cbor::*;

fn roundtrip_uint(value: u64) {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_uint(value).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.read_uint().unwrap(), value);
}

#[test]
fn uint_roundtrips() {
    for v in [
        0,
        1,
        23,
        24,
        255,
        256,
        65535,
        65536,
        u32::MAX as u64,
        u64::MAX,
    ] {
        roundtrip_uint(v);
    }
}

#[test]
fn int_roundtrips() {
    for v in [
        0i64,
        1,
        -1,
        23,
        -24,
        127,
        -128,
        1000,
        -1000,
        i64::MAX,
        i64::MIN,
    ] {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.write_int(v).unwrap();
        }
        let mut dec = Decoder::new(buf.as_slice());
        assert_eq!(dec.read_int().unwrap(), v);
    }
}

#[test]
fn bstr_roundtrip() {
    let data = b"hello world";
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_bstr(data).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.read_bstr().unwrap(), data);
}

#[test]
fn bstr_header_manual_read() {
    let data = b"payload bytes";
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_bstr(data).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    let len = dec.read_bstr_header().unwrap();
    assert_eq!(len, data.len() as u64);

    let mut payload = vec![0u8; len as usize];
    dec.inner().read_exact(&mut payload).unwrap();
    dec.advance(len);
    assert_eq!(payload, data);
    assert_eq!(dec.position(), buf.len() as u64);
}

#[test]
fn tstr_roundtrip() {
    let s = "hello 🌍";
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_tstr(s).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.read_tstr().unwrap(), s);
}

#[test]
fn array_roundtrip() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_array(3).unwrap();
        enc.write_uint(1).unwrap();
        enc.write_uint(2).unwrap();
        enc.write_uint(3).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.read_array_len().unwrap(), 3);
    assert_eq!(dec.read_uint().unwrap(), 1);
    assert_eq!(dec.read_uint().unwrap(), 2);
    assert_eq!(dec.read_uint().unwrap(), 3);
}

#[test]
fn indefinite_array_roundtrip() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_indefinite_array().unwrap();
        enc.write_uint(10).unwrap();
        enc.write_uint(20).unwrap();
        enc.write_break().unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    dec.read_indefinite_array_start().unwrap();
    assert_eq!(dec.read_uint().unwrap(), 10);
    assert_eq!(dec.read_uint().unwrap(), 20);
    assert!(dec.is_break().unwrap());
    dec.read_break().unwrap();
}

#[test]
fn map_roundtrip() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_map(2).unwrap();
        enc.write_tstr("a").unwrap();
        enc.write_uint(1).unwrap();
        enc.write_tstr("b").unwrap();
        enc.write_uint(2).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.read_map_len().unwrap(), 2);
    assert_eq!(dec.read_tstr().unwrap(), "a");
    assert_eq!(dec.read_uint().unwrap(), 1);
    assert_eq!(dec.read_tstr().unwrap(), "b");
    assert_eq!(dec.read_uint().unwrap(), 2);
}

#[test]
fn tag_roundtrip() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_tag(24).unwrap();
        enc.write_bstr(b"tagged").unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.read_tag().unwrap(), 24);
    assert_eq!(dec.read_bstr().unwrap(), b"tagged");
}

#[test]
fn bool_roundtrip() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_bool(true).unwrap();
        enc.write_bool(false).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert!(dec.read_bool().unwrap());
    assert!(!dec.read_bool().unwrap());
}

#[test]
fn simple_value_encoding() {
    use hex_literal::hex;

    // simple value 22 (null) = 0xF6
    let mut buf = Vec::new();
    Encoder::new(&mut buf).write_simple(22).unwrap();
    assert_eq!(buf, hex!("f6"));

    // simple value 16 = 0xF0
    buf.clear();
    Encoder::new(&mut buf).write_simple(16).unwrap();
    assert_eq!(buf, hex!("f0"));

    // simple value 255 = 0xF8 0xFF
    buf.clear();
    Encoder::new(&mut buf).write_simple(255).unwrap();
    assert_eq!(buf, hex!("f8ff"));
}

#[test]
fn float_roundtrips() {
    for v in [0.0f64, 1.0, -1.0, 3.14, f64::INFINITY, f64::NEG_INFINITY] {
        let mut buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut buf);
            enc.write_float_canonical(v).unwrap();
        }
        let mut dec = Decoder::new(buf.as_slice());
        assert_eq!(dec.read_float().unwrap(), v);
    }
}

#[test]
fn float_nan() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_float_canonical(f64::NAN).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    assert!(dec.read_float().unwrap().is_nan());
}

#[test]
fn skip_value_nested() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_array(3).unwrap();
        enc.write_uint(1).unwrap();
        enc.write_array(2).unwrap();
        enc.write_uint(2).unwrap();
        enc.write_uint(3).unwrap();
        enc.write_tstr("hello").unwrap();
        enc.write_uint(42).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    dec.skip_value(128).unwrap();
    assert_eq!(dec.read_uint().unwrap(), 42);
}

#[test]
fn skip_bytes() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_bstr(&[0u8; 1000]).unwrap();
        enc.write_uint(99).unwrap();
    }
    let mut dec = Decoder::new(buf.as_slice());
    dec.skip_value(128).unwrap();
    assert_eq!(dec.read_uint().unwrap(), 99);
}

#[test]
fn position_tracking() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_uint(42).unwrap();
        assert_eq!(enc.position(), 2); // 0x18 0x2a
        enc.write_bstr(b"hello").unwrap();
        let expected = 2 + 1 + 5; // uint(2) + bstr_header(1) + data(5)
        assert_eq!(enc.position(), expected as u64);
    }

    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.position(), 0);
    dec.read_uint().unwrap();
    assert_eq!(dec.position(), 2);
    dec.read_bstr().unwrap();
    assert_eq!(dec.position(), buf.len() as u64);
}

#[test]
fn cross_compat_buffer_encode_stream_decode() {
    let mut enc = hardy_cbor::buffer::encoder::BufferEncoder::new();
    enc.emit_array(Some(3), |a| {
        a.emit(&10u64);
        a.emit("test");
        a.emit(&true);
    });
    let buf = enc.build();

    let mut dec = Decoder::new(buf.as_slice());
    assert_eq!(dec.read_array_len().unwrap(), 3);
    assert_eq!(dec.read_uint().unwrap(), 10);
    assert_eq!(dec.read_tstr().unwrap(), "test");
    assert!(dec.read_bool().unwrap());
}

#[test]
fn cross_compat_stream_encode_buffer_decode() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_array(2).unwrap();
        enc.write_uint(42).unwrap();
        enc.write_uint(99).unwrap();
    }

    let (result, _len) = hardy_cbor::buffer::decoder::parse_array(
        &buf,
        |a, _shortest, _tags| -> Result<_, hardy_cbor::buffer::decoder::Error> {
            let x: u64 = a.parse()?;
            let y: u64 = a.parse()?;
            Ok((x, y))
        },
    )
    .unwrap();
    assert_eq!(result.0, 42);
    assert_eq!(result.1, 99);
}

#[test]
fn capture_value_roundtrip() {
    let mut buf = Vec::new();
    {
        let mut enc = Encoder::new(&mut buf);
        enc.write_array(2).unwrap();
        enc.write_uint(1).unwrap();
        enc.write_tstr("two").unwrap();
        enc.write_uint(99).unwrap();
    }

    let mut dec = Decoder::new(buf.as_slice());
    let captured = dec.capture_value(128).unwrap();
    assert_eq!(dec.read_uint().unwrap(), 99);

    let (result, _len) = hardy_cbor::buffer::decoder::parse_array(
        &captured,
        |a, _shortest, _tags| -> Result<_, hardy_cbor::buffer::decoder::Error> {
            let x: u64 = a.parse()?;
            a.skip_value(16)?; // skip "two"
            Ok(x)
        },
    )
    .unwrap();
    assert_eq!(result, 1);
}

/// RFC 8949 Appendix A — stream encoder must produce identical bytes.
#[test]
fn rfc8949_stream_encode() {
    use hex_literal::hex;

    fn enc(f: impl FnOnce(&mut Encoder<&mut Vec<u8>>)) -> Vec<u8> {
        let mut buf = Vec::new();
        f(&mut Encoder::new(&mut buf));
        buf
    }

    // Unsigned integers
    assert_eq!(
        enc(|e| {
            e.write_uint(0).unwrap();
        }),
        hex!("00")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(1).unwrap();
        }),
        hex!("01")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(10).unwrap();
        }),
        hex!("0a")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(23).unwrap();
        }),
        hex!("17")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(24).unwrap();
        }),
        hex!("1818")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(25).unwrap();
        }),
        hex!("1819")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(100).unwrap();
        }),
        hex!("1864")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(1000).unwrap();
        }),
        hex!("1903e8")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(1000000).unwrap();
        }),
        hex!("1a000f4240")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(1000000000000).unwrap();
        }),
        hex!("1b000000e8d4a51000")
    );
    assert_eq!(
        enc(|e| {
            e.write_uint(u64::MAX).unwrap();
        }),
        hex!("1bffffffffffffffff")
    );

    // Negative integers
    assert_eq!(
        enc(|e| {
            e.write_int(-1).unwrap();
        }),
        hex!("20")
    );
    assert_eq!(
        enc(|e| {
            e.write_int(-10).unwrap();
        }),
        hex!("29")
    );
    assert_eq!(
        enc(|e| {
            e.write_int(-100).unwrap();
        }),
        hex!("3863")
    );
    assert_eq!(
        enc(|e| {
            e.write_int(-1000).unwrap();
        }),
        hex!("3903e7")
    );

    // Floats (canonical)
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(0.0).unwrap();
        }),
        hex!("f90000")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(-0.0).unwrap();
        }),
        hex!("f98000")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(1.0).unwrap();
        }),
        hex!("f93c00")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(1.1).unwrap();
        }),
        hex!("fb3ff199999999999a")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(1.5).unwrap();
        }),
        hex!("f93e00")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(65504.0).unwrap();
        }),
        hex!("f97bff")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(100000.0).unwrap();
        }),
        hex!("fa47c35000")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(3.4028234663852886e+38).unwrap();
        }),
        hex!("fa7f7fffff")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(1.0e+300).unwrap();
        }),
        hex!("fb7e37e43c8800759c")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(5.960464477539063e-8).unwrap();
        }),
        hex!("f90001")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(0.00006103515625).unwrap();
        }),
        hex!("f90400")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(-4.0).unwrap();
        }),
        hex!("f9c400")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(-4.1).unwrap();
        }),
        hex!("fbc010666666666666")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(f64::INFINITY).unwrap();
        }),
        hex!("f97c00")
    );
    assert_eq!(
        enc(|e| {
            e.write_float_canonical(f64::NEG_INFINITY).unwrap();
        }),
        hex!("f9fc00")
    );

    // Booleans and simple values
    assert_eq!(
        enc(|e| {
            e.write_bool(false).unwrap();
        }),
        hex!("f4")
    );
    assert_eq!(
        enc(|e| {
            e.write_bool(true).unwrap();
        }),
        hex!("f5")
    );
    assert_eq!(
        enc(|e| {
            e.write_null().unwrap();
        }),
        hex!("f6")
    );
    assert_eq!(
        enc(|e| {
            e.write_undefined().unwrap();
        }),
        hex!("f7")
    );
    assert_eq!(
        enc(|e| {
            e.write_simple(16).unwrap();
        }),
        hex!("f0")
    );
    assert_eq!(
        enc(|e| {
            e.write_simple(255).unwrap();
        }),
        hex!("f8ff")
    );

    // Tags
    assert_eq!(
        enc(|e| {
            e.write_tag(0).unwrap();
            e.write_tstr("2013-03-21T20:04:00Z").unwrap();
        }),
        hex!("c074323031332d30332d32315432303a30343a30305a")
    );
    assert_eq!(
        enc(|e| {
            e.write_tag(1).unwrap();
            e.write_uint(1363896240).unwrap();
        }),
        hex!("c11a514b67b0")
    );
    assert_eq!(
        enc(|e| {
            e.write_tag(23).unwrap();
            e.write_bstr(&hex!("01020304")).unwrap();
        }),
        hex!("d74401020304")
    );
    assert_eq!(
        enc(|e| {
            e.write_tag(24).unwrap();
            e.write_bstr(&hex!("6449455446")).unwrap();
        }),
        hex!("d818456449455446")
    );
    assert_eq!(
        enc(|e| {
            e.write_tag(32).unwrap();
            e.write_tstr("http://www.example.com").unwrap();
        }),
        hex!("d82076687474703a2f2f7777772e6578616d706c652e636f6d")
    );

    // Byte strings
    assert_eq!(
        enc(|e| {
            e.write_bstr(&[]).unwrap();
        }),
        hex!("40")
    );
    assert_eq!(
        enc(|e| {
            e.write_bstr(&hex!("01020304")).unwrap();
        }),
        hex!("4401020304")
    );

    // Text strings
    assert_eq!(
        enc(|e| {
            e.write_tstr("").unwrap();
        }),
        hex!("60")
    );
    assert_eq!(
        enc(|e| {
            e.write_tstr("a").unwrap();
        }),
        hex!("6161")
    );
    assert_eq!(
        enc(|e| {
            e.write_tstr("IETF").unwrap();
        }),
        hex!("6449455446")
    );
    assert_eq!(
        enc(|e| {
            e.write_tstr("\"\\").unwrap();
        }),
        hex!("62225c")
    );
    assert_eq!(
        enc(|e| {
            e.write_tstr("\u{00fc}").unwrap();
        }),
        hex!("62c3bc")
    );
    assert_eq!(
        enc(|e| {
            e.write_tstr("\u{6c34}").unwrap();
        }),
        hex!("63e6b0b4")
    );
    assert_eq!(
        enc(|e| {
            e.write_tstr("\u{10151}").unwrap();
        }),
        hex!("64f0908591")
    );

    // Definite arrays
    assert_eq!(
        enc(|e| {
            e.write_array(0).unwrap();
        }),
        hex!("80")
    );
    assert_eq!(
        enc(|e| {
            e.write_array(3).unwrap();
            e.write_uint(1).unwrap();
            e.write_uint(2).unwrap();
            e.write_uint(3).unwrap();
        }),
        hex!("83010203")
    );
    assert_eq!(
        enc(|e| {
            e.write_array(25).unwrap();
            for i in 1..=25u64 {
                e.write_uint(i).unwrap();
            }
        }),
        hex!("98190102030405060708090a0b0c0d0e0f101112131415161718181819")
    );

    // Definite maps
    assert_eq!(
        enc(|e| {
            e.write_map(0).unwrap();
        }),
        hex!("a0")
    );
    assert_eq!(
        enc(|e| {
            e.write_map(2).unwrap();
            e.write_uint(1).unwrap();
            e.write_uint(2).unwrap();
            e.write_uint(3).unwrap();
            e.write_uint(4).unwrap();
        }),
        hex!("a201020304")
    );
    assert_eq!(
        enc(|e| {
            e.write_map(5).unwrap();
            for (k, v) in [("a", "A"), ("b", "B"), ("c", "C"), ("d", "D"), ("e", "E")] {
                e.write_tstr(k).unwrap();
                e.write_tstr(v).unwrap();
            }
        }),
        hex!("a56161614161626142616361436164614461656145")
    );

    // Indefinite arrays
    assert_eq!(
        enc(|e| {
            e.write_indefinite_array().unwrap();
            e.write_break().unwrap();
        }),
        hex!("9fff")
    );
    assert_eq!(
        enc(|e| {
            e.write_indefinite_array().unwrap();
            for i in 1..=25u64 {
                e.write_uint(i).unwrap();
            }
            e.write_break().unwrap();
        }),
        hex!("9f0102030405060708090a0b0c0d0e0f101112131415161718181819ff")
    );

    // Indefinite maps
    assert_eq!(
        enc(|e| {
            e.write_indefinite_map().unwrap();
            e.write_tstr("Fun").unwrap();
            e.write_bool(true).unwrap();
            e.write_tstr("Amt").unwrap();
            e.write_int(-2).unwrap();
            e.write_break().unwrap();
        }),
        hex!("bf6346756ef563416d7421ff")
    );

    // Indefinite byte string
    assert_eq!(
        enc(|e| {
            e.write_indefinite_bstr().unwrap();
            e.write_bstr(&hex!("0102")).unwrap();
            e.write_bstr(&hex!("030405")).unwrap();
            e.write_break().unwrap();
        }),
        hex!("5f42010243030405ff")
    );

    // Indefinite text string
    assert_eq!(
        enc(|e| {
            e.write_indefinite_tstr().unwrap();
            e.write_tstr("strea").unwrap();
            e.write_tstr("ming").unwrap();
            e.write_break().unwrap();
        }),
        hex!("7f657374726561646d696e67ff")
    );
}

/// RFC 8949 Appendix A — stream decoder must read all test vectors.
#[test]
fn rfc8949_stream_decode() {
    use hex_literal::hex;

    // Unsigned integers
    assert_eq!(Decoder::new(hex!("00").as_slice()).read_uint().unwrap(), 0);
    assert_eq!(Decoder::new(hex!("01").as_slice()).read_uint().unwrap(), 1);
    assert_eq!(Decoder::new(hex!("0a").as_slice()).read_uint().unwrap(), 10);
    assert_eq!(Decoder::new(hex!("17").as_slice()).read_uint().unwrap(), 23);
    assert_eq!(
        Decoder::new(hex!("1818").as_slice()).read_uint().unwrap(),
        24
    );
    assert_eq!(
        Decoder::new(hex!("1903e8").as_slice()).read_uint().unwrap(),
        1000
    );
    assert_eq!(
        Decoder::new(hex!("1a000f4240").as_slice())
            .read_uint()
            .unwrap(),
        1000000
    );
    assert_eq!(
        Decoder::new(hex!("1bffffffffffffffff").as_slice())
            .read_uint()
            .unwrap(),
        u64::MAX
    );

    // Negative integers
    assert_eq!(Decoder::new(hex!("20").as_slice()).read_int().unwrap(), -1);
    assert_eq!(Decoder::new(hex!("29").as_slice()).read_int().unwrap(), -10);
    assert_eq!(
        Decoder::new(hex!("3863").as_slice()).read_int().unwrap(),
        -100
    );
    assert_eq!(
        Decoder::new(hex!("3903e7").as_slice()).read_int().unwrap(),
        -1000
    );

    // Floats
    assert_eq!(
        Decoder::new(hex!("f90000").as_slice())
            .read_float()
            .unwrap(),
        0.0
    );
    assert_eq!(
        Decoder::new(hex!("f93c00").as_slice())
            .read_float()
            .unwrap(),
        1.0
    );
    assert_eq!(
        Decoder::new(hex!("fb3ff199999999999a").as_slice())
            .read_float()
            .unwrap(),
        1.1
    );
    assert_eq!(
        Decoder::new(hex!("f93e00").as_slice())
            .read_float()
            .unwrap(),
        1.5
    );
    assert_eq!(
        Decoder::new(hex!("f97bff").as_slice())
            .read_float()
            .unwrap(),
        65504.0
    );
    assert_eq!(
        Decoder::new(hex!("fa47c35000").as_slice())
            .read_float()
            .unwrap(),
        100000.0
    );
    assert_eq!(
        Decoder::new(hex!("f9c400").as_slice())
            .read_float()
            .unwrap(),
        -4.0
    );
    assert_eq!(
        Decoder::new(hex!("f97c00").as_slice())
            .read_float()
            .unwrap(),
        f64::INFINITY
    );
    assert!(
        Decoder::new(hex!("f97e00").as_slice())
            .read_float()
            .unwrap()
            .is_nan()
    );
    assert_eq!(
        Decoder::new(hex!("f9fc00").as_slice())
            .read_float()
            .unwrap(),
        f64::NEG_INFINITY
    );

    // Booleans and simple
    assert_eq!(
        Decoder::new(hex!("f4").as_slice()).read_bool().unwrap(),
        false
    );
    assert_eq!(
        Decoder::new(hex!("f5").as_slice()).read_bool().unwrap(),
        true
    );
    assert!(Decoder::new(hex!("f6").as_slice()).read_null().is_ok());
    assert_eq!(
        Decoder::new(hex!("f0").as_slice()).read_simple().unwrap(),
        16
    );
    assert_eq!(
        Decoder::new(hex!("f8ff").as_slice()).read_simple().unwrap(),
        255
    );

    // Tags
    let mut d = Decoder::new(hex!("c11a514b67b0").as_slice());
    assert_eq!(d.read_tag().unwrap(), 1);
    assert_eq!(d.read_uint().unwrap(), 1363896240);

    // Byte strings
    assert_eq!(
        Decoder::new(hex!("40").as_slice()).read_bstr().unwrap(),
        b""
    );
    assert_eq!(
        Decoder::new(hex!("4401020304").as_slice())
            .read_bstr()
            .unwrap(),
        hex!("01020304")
    );

    // Text strings
    assert_eq!(Decoder::new(hex!("60").as_slice()).read_tstr().unwrap(), "");
    assert_eq!(
        Decoder::new(hex!("6161").as_slice()).read_tstr().unwrap(),
        "a"
    );
    assert_eq!(
        Decoder::new(hex!("6449455446").as_slice())
            .read_tstr()
            .unwrap(),
        "IETF"
    );
    assert_eq!(
        Decoder::new(hex!("62225c").as_slice()).read_tstr().unwrap(),
        "\"\\"
    );
    assert_eq!(
        Decoder::new(hex!("62c3bc").as_slice()).read_tstr().unwrap(),
        "\u{00fc}"
    );
    assert_eq!(
        Decoder::new(hex!("63e6b0b4").as_slice())
            .read_tstr()
            .unwrap(),
        "\u{6c34}"
    );
    assert_eq!(
        Decoder::new(hex!("64f0908591").as_slice())
            .read_tstr()
            .unwrap(),
        "\u{10151}"
    );

    // Definite arrays
    assert_eq!(
        Decoder::new(hex!("80").as_slice())
            .read_array_len()
            .unwrap(),
        0
    );
    let mut d = Decoder::new(hex!("83010203").as_slice());
    assert_eq!(d.read_array_len().unwrap(), 3);
    assert_eq!(d.read_uint().unwrap(), 1);
    assert_eq!(d.read_uint().unwrap(), 2);
    assert_eq!(d.read_uint().unwrap(), 3);

    // Definite maps
    assert_eq!(
        Decoder::new(hex!("a0").as_slice()).read_map_len().unwrap(),
        0
    );
    let mut d = Decoder::new(hex!("a201020304").as_slice());
    assert_eq!(d.read_map_len().unwrap(), 2);
    assert_eq!(d.read_uint().unwrap(), 1);
    assert_eq!(d.read_uint().unwrap(), 2);
    assert_eq!(d.read_uint().unwrap(), 3);
    assert_eq!(d.read_uint().unwrap(), 4);

    // Indefinite arrays
    let mut d = Decoder::new(hex!("9fff").as_slice());
    d.read_indefinite_array_start().unwrap();
    assert!(d.is_break().unwrap());
    d.read_break().unwrap();

    // Indefinite byte string
    assert_eq!(
        Decoder::new(hex!("5f42010243030405ff").as_slice())
            .read_bstr()
            .unwrap(),
        hex!("0102030405")
    );

    // Indefinite text string
    assert_eq!(
        Decoder::new(hex!("7f657374726561646d696e67ff").as_slice())
            .read_tstr()
            .unwrap(),
        "streaming"
    );
}

#[test]
fn shared_head_encoding_matches() {
    for v in [
        0u64,
        23,
        24,
        255,
        256,
        65535,
        65536,
        u32::MAX as u64,
        u64::MAX,
    ] {
        let mut stream_buf = Vec::new();
        {
            let mut enc = Encoder::new(&mut stream_buf);
            enc.write_uint(v).unwrap();
        }

        let mut buffer_enc = hardy_cbor::buffer::encoder::BufferEncoder::new();
        buffer_enc.emit(&v);
        let buffer_buf = buffer_enc.build();

        assert_eq!(stream_buf, buffer_buf, "mismatch for value {v}");
    }
}
