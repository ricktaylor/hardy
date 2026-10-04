#![no_main]

use bytes::Bytes;
use hardy_btpu::codec::{
    DecodeOptions, EncodeError, decode_pdu_with, encode_message, encoded_message_len,
    header::{HEADER_SIZE, MAX_CONTENT_LENGTH},
    message::Message,
};
use libfuzzer_sys::fuzz_target;

// Every 0x9F "bundle" is taken to be eight bytes long, so the extent hook
// path is exercised alongside the hookless one.
fn eight_byte_bundles(bytes: &[u8]) -> Option<usize> {
    (bytes.first() == Some(&0x9F) && bytes.len() >= 8).then_some(8)
}

fuzz_target!(|data: &[u8]| {
    let pdu = Bytes::copy_from_slice(data);
    let hook = &eight_byte_bundles;
    for options in [
        DecodeOptions::default(),
        DecodeOptions {
            fec: true,
            bundle_extent: None,
        },
        DecodeOptions {
            fec: true,
            bundle_extent: Some(hook),
        },
    ] {
        let mut iter = decode_pdu_with(pdu.clone(), options);
        for item in iter.by_ref() {
            let Ok(msg) = item else {
                continue;
            };
            // Everything the decoder yields must re-encode, at exactly the
            // length it predicts.  An unknown message re-encodes to the
            // bytes it was decoded from (byte-exact relay).  The one
            // exception is an encapsulated bundle longer than a message's
            // content field can hold: it was never a message, and encoding
            // it as one must refuse rather than truncate.  Reaching this
            // branch takes an input over 1 MiB, so it runs only with a
            // `-max_len` raised above libFuzzer's default of 4096.
            let mut buf = vec![0; encoded_message_len(&msg)];
            if let Message::Bundle { data, .. } = &msg
                && data.len() > MAX_CONTENT_LENGTH
            {
                assert!(matches!(
                    encode_message(&msg, &mut buf),
                    Err(EncodeError::LengthOverflow { .. })
                ));
                assert!(buf.iter().all(|&b| b == 0));
                continue;
            }
            assert_eq!(
                encode_message(&msg, &mut buf),
                Ok(buf.len()),
                "decoded messages re-encode"
            );
            if let Message::Unknown { data, .. } = &msg {
                assert_eq!(&buf[HEADER_SIZE..], data.as_ref());
            }
            // And the encoding decodes back to the same message, alone.
            let mut again = decode_pdu_with(Bytes::from(buf), options);
            assert_eq!(again.next(), Some(Ok(msg)));
            assert_eq!(again.next(), None);
        }
        assert!(iter.is_exhausted());
    }
});
