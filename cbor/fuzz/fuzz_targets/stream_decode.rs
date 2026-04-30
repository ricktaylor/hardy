#![no_main]

use libfuzzer_sys::fuzz_target;

use hardy_cbor::Decoder;

// Fuzz the streaming decoder with arbitrary bytes.
// Exercises: read_head, read_argument, peek, all read_* methods,
// skip_value (with depth limit), and capture_value.
fuzz_target!(|data: &[u8]| {
    // Limit allocations to 1 MB to avoid OOM during fuzzing.
    fn fuzz_decoder(data: &[u8]) -> Decoder<&[u8]> {
        let mut dec = Decoder::new(data);
        dec.set_max_alloc(1024 * 1024);
        dec
    }

    // 1. Try skip_value — exercises recursive container traversal
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.skip_value(32);
    }

    // 2. Try capture_value — exercises recursive capture + position tracking
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.capture_value(32);
    }

    // 3. Try reading as each major type — exercises expect_major + argument parsing
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_uint();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_int();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_bstr();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_tstr();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_array_len();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_map_len();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_tag();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_bool();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_null();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_float();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_simple();
    }
    {
        let mut dec = fuzz_decoder(data);
        _ = dec.read_uint_or_tstr();
    }

    // 4. Try bstr_header + manual skip — exercises the streaming payload path
    {
        let mut dec = fuzz_decoder(data);
        if let Ok(len) = dec.read_bstr_header() {
            _ = dec.skip(len.min(1024 * 1024));
        }
    }

    // 5. Try reading an indefinite array until break
    {
        let mut dec = fuzz_decoder(data);
        if dec.read_indefinite_array_start().is_ok() {
            for _ in 0..1000 {
                match dec.is_break() {
                    Ok(true) => {
                        _ = dec.read_break();
                        break;
                    }
                    Ok(false) => {
                        _ = dec.skip_value(16);
                    }
                    Err(_) => break,
                }
            }
        }
    }
});
