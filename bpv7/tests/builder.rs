//! Public-API tests for [`Builder::build_stream`]: the streamed build must
//! be byte-for-byte the [`Builder::build`] wire form, with a bundle view
//! whose extents span the full future wire form.

use hardy_bpv7::{
    block,
    builder::{Builder, StreamBuild},
    crc::CrcType,
    creation_timestamp::CreationTimestamp,
    eid::Eid,
    hop_info::HopInfo,
    parse,
};

fn source() -> Eid {
    "ipn:1.2".parse().unwrap()
}

fn destination() -> Eid {
    "ipn:2.1".parse().unwrap()
}

// Assemble the streamed form: prefix, then the payload (fed to the trailer
// digest in `chunk`-sized runs, or whole), then the trailer bytes.
fn assemble(sb: StreamBuild, payload: &[u8], chunk: Option<usize>) -> Vec<u8> {
    let StreamBuild {
        prefix,
        mut trailer,
        ..
    } = sb;
    match chunk {
        None => trailer.update(payload),
        Some(n) => {
            for run in payload.chunks(n) {
                trailer.update(run);
            }
        }
    }
    let mut out = prefix.into_vec();
    out.extend_from_slice(payload);
    out.extend(trailer.finish());
    out
}

// The pinning matrix: for the same inputs and timestamp,
// `prefix ++ payload ++ trailer` equals `build()` byte-for-byte, and the
// two bundle views are equal (extents included), across CRC types, payload
// sizes (empty / small / multi-chunk), and an extension block.
#[test]
fn build_stream_matches_build() {
    let large = vec![0xAB_u8; 4000];
    let payloads: [&[u8]; 3] = [b"", b"Hello", &large];
    for crc_type in [CrcType::None, CrcType::CRC16_X25, CrcType::CRC32_CASTAGNOLI] {
        for payload in payloads {
            for with_extension in [false, true] {
                let timestamp = CreationTimestamp::now();
                let make = || {
                    let mut b = Builder::new(source(), destination()).with_crc_type(crc_type);
                    if with_extension {
                        b = b.with_hop_count(&HopInfo {
                            limit: 16,
                            count: 0,
                        });
                    }
                    b
                };

                let (bundle, data) = make()
                    .with_payload(payload.into())
                    .build(timestamp.clone())
                    .unwrap();
                let sb = make()
                    .build_stream(payload.len() as u64, timestamp)
                    .unwrap();

                assert_eq!(
                    sb.bundle,
                    bundle,
                    "bundle views diverge (crc {crc_type:?}, payload {} bytes, ext {with_extension})",
                    payload.len()
                );
                assert_eq!(
                    sb.bundle.encoded_len(),
                    data.len() as u64,
                    "declared wire size must match the built form"
                );
                let assembled = assemble(sb, payload, None);
                assert_eq!(
                    assembled,
                    data.as_ref(),
                    "wire bytes diverge (crc {crc_type:?}, payload {} bytes, ext {with_extension})",
                    payload.len()
                );
            }
        }
    }
}

// A payload template configured through `with_payload` keeps its flags and
// per-block CRC type; only its resident bytes are ignored.
#[test]
fn build_stream_honours_a_configured_payload_template() {
    let payload = b"configured";
    let timestamp = CreationTimestamp::now();
    let make = || {
        Builder::new(source(), destination())
            .add_extension_block(block::Type::Payload)
            .unwrap()
            .with_crc_type(CrcType::CRC16_X25)
            .with_flags(block::Flags {
                delete_bundle_on_failure: true,
                report_on_failure: true,
                ..Default::default()
            })
    };

    let (bundle, data) = make()
        .build(payload.as_slice().into())
        .build(timestamp.clone())
        .unwrap();
    // The streamed twin supplies the template's data too — it is ignored,
    // the declared length governs.
    let sb = make()
        .build(payload.as_slice().into())
        .build_stream(payload.len() as u64, timestamp)
        .unwrap();

    assert_eq!(sb.bundle, bundle);
    assert_eq!(assemble(sb, payload, None), data.as_ref());
}

// Chunked trailer feeding is equivalent to one-shot: the digest is
// incremental.
#[test]
fn build_stream_trailer_is_chunking_independent() {
    let payload = vec![0x5A_u8; 1000];
    let timestamp = CreationTimestamp::now();
    let build = |ts: CreationTimestamp| {
        Builder::new(source(), destination())
            .build_stream(payload.len() as u64, ts)
            .unwrap()
    };

    let whole = assemble(build(timestamp.clone()), &payload, None);
    let chunked = assemble(build(timestamp), &payload, Some(7));
    assert_eq!(whole, chunked);
}

// The assembled streamed form round-trips the canonical parser, and the
// parsed view agrees with the build-time view — extents, data ranges, and
// the payload bytes themselves.
#[test]
fn build_stream_output_parses_canonically() {
    let payload = b"round trip";
    let sb = Builder::new(source(), destination())
        .build_stream(payload.len() as u64, CreationTimestamp::now())
        .unwrap();
    let view = sb.bundle.clone();

    let parsed = parse::parse(assemble(sb, payload, None).into()).expect("assembled form parses");
    assert_eq!(parsed.bundle, view, "parsed view must equal the build view");
    assert_eq!(
        parsed.bundle.blocks[&1]
            .payload(&parsed.data)
            .expect("payload resident in the parsed buffer"),
        payload
    );
}
