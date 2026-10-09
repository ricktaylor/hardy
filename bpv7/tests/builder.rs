//! Integration tests for `hardy_bpv7::builder`: the flags a built bundle
//! carries on the wire, read back through the parser, and
//! [`Builder::build_stream`], which must be byte-for-byte the
//! [`Builder::build`] wire form, with a bundle view whose extents span the
//! full future wire form.

use core::num::NonZeroU8;

use bytes::Bytes;
use hardy_bpv7::{
    block,
    builder::{self, Builder, StreamBuild},
    bundle,
    crc::CrcType,
    creation_timestamp::{self, CreationTimestamp},
    eid::Eid,
    hop_info::{self, HopInfo},
    parse,
};

// A bundle that forbids `report_on_failure` (RFC 9171 §4.2.3-4/-5) is built
// with the flag clear on every block — the Hop Count block's default and
// caller-set flags on an extension block and on the payload alike — so it
// parses; an ordinary bundle keeps all three.
#[test]
fn builder_clears_report_on_failure_on_a_forbidding_bundle() {
    let hop_info = hop_info::HopInfo {
        limit: NonZeroU8::new(30).unwrap(),
        count: 0,
    };
    let reporting = || block::Flags {
        report_on_failure: true,
        ..Default::default()
    };
    for (label, source, flags, reports) in [
        (
            "null source",
            "dtn:none",
            bundle::Flags {
                do_not_fragment: true,
                ..Default::default()
            },
            false,
        ),
        (
            "admin record",
            "ipn:1.0",
            bundle::Flags {
                is_admin_record: true,
                ..Default::default()
            },
            false,
        ),
        ("ordinary", "ipn:1.0", bundle::Flags::default(), true),
    ] {
        let (_, data) = builder::Builder::new(source.parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_flags(flags)
            .with_hop_count(&hop_info)
            .add_extension_block(block::Type::Unrecognised(200))
            .unwrap()
            .with_flags(reporting())
            .build(b"ext-data".as_slice().into())
            .add_extension_block(block::Type::Payload)
            .unwrap()
            .with_flags(reporting())
            .build(b"Hello".as_slice().into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap();
        let parsed = parse::parse(Bytes::from(data))
            .unwrap_or_else(|e| panic!("{label} bundle must parse: {e:?}"))
            .bundle;
        for block_type in [
            block::Type::HopCount,
            block::Type::Unrecognised(200),
            block::Type::Payload,
        ] {
            let block = parsed
                .blocks
                .values()
                .find(|b| b.block_type == block_type)
                .expect("the block is present");
            assert_eq!(
                block.flags.report_on_failure, reports,
                "{label}: {block_type:?} reports on failure exactly when the bundle allows it"
            );
        }
    }
}

// The builder canonicalizes the flags it is handed, at both flag levels: a
// named bit carried in `unrecognised` is the flag it encodes — the
// admin-record alias makes an administrative record, whose normalisation
// then clears the Hop Count's `report_on_failure`, and `report_on_failure`'s
// alias reports on an ordinary bundle's block — while genuinely unrecognised
// bits pass through. The returned `Bundle` holds what the bytes hold.
#[test]
fn builder_canonicalizes_unrecognised_aliases_of_named_flags() {
    let (built, data) =
        builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
            .with_flags(bundle::Flags {
                unrecognised: (1 << 1) | (1 << 24),
                ..Default::default()
            })
            .with_hop_count(&hop_info::HopInfo {
                limit: NonZeroU8::new(30).unwrap(),
                count: 0,
            })
            .with_payload(b"Hello".as_slice().into())
            .build(creation_timestamp::CreationTimestamp::now())
            .unwrap();
    let parsed = parse::parse(Bytes::from(data))
        .expect("the administrative record's blocks request no report")
        .bundle;
    assert!(parsed.primary.flags.is_admin_record);
    assert_eq!(parsed.primary.flags.unrecognised, 1 << 24);
    assert!(built.primary.flags.is_canonical());
    assert_eq!(built.primary.flags, parsed.primary.flags);
    let hop_count = parsed
        .blocks
        .values()
        .find(|b| b.block_type == block::Type::HopCount)
        .expect("the Hop Count block is present");
    assert!(
        !hop_count.flags.report_on_failure,
        "an administrative record clears the Hop Count default"
    );

    for (label, bundle_flags, reports) in [
        ("ordinary", bundle::Flags::default(), true),
        (
            "admin record",
            bundle::Flags {
                is_admin_record: true,
                ..Default::default()
            },
            false,
        ),
    ] {
        let (built, data) =
            builder::Builder::new("ipn:1.0".parse().unwrap(), "ipn:2.0".parse().unwrap())
                .with_flags(bundle_flags)
                .add_extension_block(block::Type::Unrecognised(200))
                .unwrap()
                .with_flags(block::Flags {
                    unrecognised: (1 << 1) | (1 << 8),
                    ..Default::default()
                })
                .build(b"ext-data".as_slice().into())
                .with_payload(b"Hello".as_slice().into())
                .build(creation_timestamp::CreationTimestamp::now())
                .unwrap();
        let parsed = parse::parse(Bytes::from(data))
            .unwrap_or_else(|e| panic!("{label}: the bundle must parse: {e:?}"))
            .bundle;
        let block = parsed
            .blocks
            .values()
            .find(|b| b.block_type == block::Type::Unrecognised(200))
            .unwrap_or_else(|| panic!("{label}: the extension block is present"));
        assert_eq!(
            block.flags.report_on_failure, reports,
            "{label}: the alias reports exactly when the bundle allows it"
        );
        assert_eq!(block.flags.unrecognised, 1 << 8, "{label}");
        let built_block = built
            .blocks
            .values()
            .find(|b| b.block_type == block::Type::Unrecognised(200))
            .unwrap_or_else(|| panic!("{label}: the built view has the block"));
        assert!(built_block.flags.is_canonical(), "{label}");
        assert_eq!(built_block.flags, block.flags, "{label}");
    }
}

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
                            limit: NonZeroU8::new(16).unwrap(),
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

// A bundle that forbids `report_on_failure` (RFC 9171 §4.2.3-4/-5) streams
// exactly as it builds: the flag is cleared on every block — the Hop Count
// default and a caller-set flag alike — and the assembled form parses.
#[test]
fn build_stream_clears_report_on_failure_like_build() {
    for (source, flags) in [
        (
            "dtn:none",
            bundle::Flags {
                do_not_fragment: true,
                ..Default::default()
            },
        ),
        (
            "ipn:1.2",
            bundle::Flags {
                is_admin_record: true,
                ..Default::default()
            },
        ),
    ] {
        let timestamp = CreationTimestamp::now();
        let make = || {
            Builder::new(source.parse().unwrap(), destination())
                .with_flags(flags.clone())
                .with_hop_count(&HopInfo {
                    limit: NonZeroU8::new(30).unwrap(),
                    count: 0,
                })
                .add_extension_block(block::Type::Unrecognised(200))
                .unwrap()
                .with_flags(block::Flags {
                    report_on_failure: true,
                    ..Default::default()
                })
                .build(b"ext-data".as_slice().into())
        };
        let payload = b"Hello";
        let (_, built) = make()
            .with_payload(payload.as_slice().into())
            .build(timestamp.clone())
            .unwrap();
        let streamed = assemble(
            make()
                .build_stream(payload.len() as u64, timestamp)
                .unwrap(),
            payload,
            None,
        );
        assert_eq!(streamed, built.as_ref(), "{source}: stream and build agree");

        let parsed = parse::parse(streamed.into())
            .unwrap_or_else(|e| panic!("{source}: the streamed bundle must parse: {e:?}"))
            .bundle;
        assert!(
            parsed
                .blocks
                .values()
                .filter(|b| b.block_type != block::Type::Primary)
                .all(|b| !b.flags.report_on_failure),
            "{source}: no canonical block requests a report on failure"
        );
    }
}
