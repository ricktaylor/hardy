#![no_main]

use libfuzzer_sys::fuzz_target;

use hardy_cbor::{Decoder, Encoder, Read};

// Fuzz encode→decode roundtrip: encode structured data from fuzzer input,
// then decode and verify consistency.
fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }

    // Use first byte as a "command" to decide what to encode
    let cmd = data[0];
    let payload = &data[1..];

    let mut encoded = Vec::new();

    match cmd % 8 {
        0 => {
            // uint roundtrip
            if payload.len() >= 8 {
                let v = u64::from_le_bytes(payload[..8].try_into().unwrap());
                {
                    let mut enc = Encoder::new(&mut encoded);
                    if enc.write_uint(v).is_err() {
                        return;
                    }
                }
                let mut dec = Decoder::new(encoded.as_slice());
                assert_eq!(dec.read_uint().unwrap(), v);
            }
        }
        1 => {
            // int roundtrip
            if payload.len() >= 8 {
                let v = i64::from_le_bytes(payload[..8].try_into().unwrap());
                {
                    let mut enc = Encoder::new(&mut encoded);
                    if enc.write_int(v).is_err() {
                        return;
                    }
                }
                let mut dec = Decoder::new(encoded.as_slice());
                assert_eq!(dec.read_int().unwrap(), v);
            }
        }
        2 => {
            // bstr roundtrip
            {
                let mut enc = Encoder::new(&mut encoded);
                if enc.write_bstr(payload).is_err() {
                    return;
                }
            }
            let mut dec = Decoder::new(encoded.as_slice());
            assert_eq!(dec.read_bstr().unwrap(), payload);
        }
        3 => {
            // tstr roundtrip (only if valid UTF-8)
            if let Ok(s) = core::str::from_utf8(payload) {
                {
                    let mut enc = Encoder::new(&mut encoded);
                    if enc.write_tstr(s).is_err() {
                        return;
                    }
                }
                let mut dec = Decoder::new(encoded.as_slice());
                assert_eq!(dec.read_tstr().unwrap(), s);
            }
        }
        4 => {
            // array of uints roundtrip
            let count = (payload.len() / 8).min(100);
            {
                let mut enc = Encoder::new(&mut encoded);
                if enc.write_array(count).is_err() {
                    return;
                }
                for i in 0..count {
                    let v = u64::from_le_bytes(payload[i * 8..(i + 1) * 8].try_into().unwrap());
                    if enc.write_uint(v).is_err() {
                        return;
                    }
                }
            }
            let mut dec = Decoder::new(encoded.as_slice());
            let n = dec.read_array_len().unwrap();
            assert_eq!(n, count);
            for i in 0..count {
                let expected =
                    u64::from_le_bytes(payload[i * 8..(i + 1) * 8].try_into().unwrap());
                assert_eq!(dec.read_uint().unwrap(), expected);
            }
        }
        5 => {
            // tag + bstr roundtrip
            if payload.len() >= 8 {
                let tag = u64::from_le_bytes(payload[..8].try_into().unwrap());
                let bstr_data = &payload[8..];
                {
                    let mut enc = Encoder::new(&mut encoded);
                    if enc.write_tag(tag).is_err() || enc.write_bstr(bstr_data).is_err() {
                        return;
                    }
                }
                let mut dec = Decoder::new(encoded.as_slice());
                assert_eq!(dec.read_tag().unwrap(), tag);
                assert_eq!(dec.read_bstr().unwrap(), bstr_data);
            }
        }
        6 => {
            // bstr_header + manual read roundtrip (streaming path)
            {
                let mut enc = Encoder::new(&mut encoded);
                if enc.write_bstr(payload).is_err() {
                    return;
                }
            }
            let mut dec = Decoder::new(encoded.as_slice());
            let len = dec.read_bstr_header().unwrap();
            assert_eq!(len, payload.len() as u64);
            let mut readback = vec![0u8; len as usize];
            dec.inner().read_exact(&mut readback).unwrap();
            dec.advance(len);
            assert_eq!(readback, payload);
            assert_eq!(dec.position(), encoded.len() as u64);
        }
        7 => {
            // capture_value roundtrip: encode, capture, re-parse with buffer decoder
            {
                let mut enc = Encoder::new(&mut encoded);
                if enc.write_array(2).is_err()
                    || enc.write_uint(42).is_err()
                    || enc.write_bstr(payload).is_err()
                {
                    return;
                }
            }
            let mut dec = Decoder::new(encoded.as_slice());
            let captured = dec.capture_value(128).unwrap();
            // The captured bytes should be valid CBOR
            let (_, _len) = hardy_cbor::buffer::decoder::parse_array(
                &captured,
                |a, _, _| -> Result<(), hardy_cbor::buffer::decoder::Error> {
                    let _: u64 = a.parse()?;
                    a.skip_value(16)?;
                    Ok(())
                },
            )
            .unwrap();
        }
        _ => unreachable!(),
    }
});
