use hardy_bpv7::{
    bundle, creation_timestamp, dtn_time,
    status_report::{AdministrativeRecord, BundleStatusReport, Error, ReasonCode, StatusAssertion},
};
use hardy_cbor::decode::FromCbor;

fn roundtrip_report(report: &BundleStatusReport) -> BundleStatusReport {
    let encoded = hardy_cbor::encode::emit(report);
    let (decoded, _, _) =
        BundleStatusReport::from_cbor(&encoded.0).expect("Should decode status report");
    decoded
}

fn roundtrip_admin(record: &AdministrativeRecord) -> AdministrativeRecord {
    let encoded = hardy_cbor::encode::emit(record);
    let (decoded, _, _) =
        AdministrativeRecord::from_cbor(&encoded.0).expect("Should decode admin record");
    decoded
}

// PICS Items 47, 48: Administrative record and status report formatting
#[test]
fn minimal_status_report_roundtrip() {
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:1.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: None,
        },
        reason: ReasonCode::NoAdditionalInformation,
        ..Default::default()
    };

    let decoded = roundtrip_report(&report);
    assert_eq!(decoded.bundle_id.source, report.bundle_id.source);
    assert_eq!(decoded.reason, ReasonCode::NoAdditionalInformation);
    assert!(decoded.received.is_none());
    assert!(decoded.forwarded.is_none());
    assert!(decoded.delivered.is_none());
    assert!(decoded.deleted.is_none());
}

// PICS Item 43: Bundle deletion status report
#[test]
fn deletion_report() {
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:10.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: None,
        },
        deleted: Some(StatusAssertion(None)),
        reason: ReasonCode::LifetimeExpired,
        ..Default::default()
    };

    let decoded = roundtrip_report(&report);
    assert!(decoded.deleted.is_some());
    assert!(decoded.received.is_none());
    assert_eq!(decoded.reason, ReasonCode::LifetimeExpired);
}

#[test]
fn all_assertions_set() {
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:1.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: None,
        },
        received: Some(StatusAssertion(None)),
        forwarded: Some(StatusAssertion(None)),
        delivered: Some(StatusAssertion(None)),
        deleted: Some(StatusAssertion(None)),
        reason: ReasonCode::NoAdditionalInformation,
    };

    let decoded = roundtrip_report(&report);
    assert!(decoded.received.is_some());
    assert!(decoded.forwarded.is_some());
    assert!(decoded.delivered.is_some());
    assert!(decoded.deleted.is_some());
}

#[test]
fn fragment_info_roundtrip() {
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:1.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: Some(bundle::FragmentInfo {
                offset: 1000,
                total_adu_length: 5000,
            }),
        },
        received: Some(StatusAssertion(None)),
        reason: ReasonCode::NoAdditionalInformation,
        ..Default::default()
    };

    let decoded = roundtrip_report(&report);
    let frag = decoded
        .bundle_id
        .fragment_info
        .expect("Fragment info should survive roundtrip");
    assert_eq!(frag.offset, 1000);
    assert_eq!(frag.total_adu_length, 5000);
}

#[test]
fn administrative_record_roundtrip() {
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:1.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: None,
        },
        delivered: Some(StatusAssertion(None)),
        reason: ReasonCode::NoAdditionalInformation,
        ..Default::default()
    };

    let record = AdministrativeRecord::BundleStatusReport(report);
    let decoded = roundtrip_admin(&record);
    match decoded {
        AdministrativeRecord::BundleStatusReport(r) => {
            assert!(r.delivered.is_some());
            assert_eq!(r.bundle_id.source, "ipn:1.0".parse().unwrap());
        }
    }
}

// RFC 9171 §6.1.1: a status assertion may carry the time of the asserted
// event. The timestamp must survive the encode/decode round trip.
#[test]
fn timestamped_assertion_roundtrip() {
    let event_time: time::OffsetDateTime = dtn_time::DtnTime::new(820_000_000_000).into();
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:1.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: None,
        },
        received: Some(StatusAssertion(Some(event_time))),
        reason: ReasonCode::NoAdditionalInformation,
        ..Default::default()
    };

    let decoded = roundtrip_report(&report);
    let Some(StatusAssertion(Some(decoded_time))) = decoded.received else {
        panic!(
            "Timestamped received assertion should survive round trip, got: {:?}",
            decoded.received
        );
    };
    assert_eq!(decoded_time, event_time);
    assert!(decoded.forwarded.is_none());
    assert!(decoded.delivered.is_none());
    assert!(decoded.deleted.is_none());
}

// A wire assertion of [true, 0] (status asserted, zero DTN time) decodes
// to an assertion without a timestamp: zero means "no time available".
#[test]
fn zero_timestamp_assertion_decodes_without_time() {
    // The DTN epoch encodes as DTN time 0, so this emits [true, 0].
    let epoch: time::OffsetDateTime = dtn_time::DtnTime::new(0).into();
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:1.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: None,
        },
        delivered: Some(StatusAssertion(Some(epoch))),
        reason: ReasonCode::NoAdditionalInformation,
        ..Default::default()
    };

    let decoded = roundtrip_report(&report);
    assert!(
        matches!(decoded.delivered, Some(StatusAssertion(None))),
        "[true, 0] should decode to an assertion without a timestamp, got: {:?}",
        decoded.delivered
    );
}

// Set exactly one of the four assertions per iteration and check it comes
// back in the same position: any pairwise swap in the emit or parse order
// (e.g. received/forwarded) fails this test.
#[test]
fn assertion_positions_roundtrip() {
    for position in 0..4 {
        let mut report = BundleStatusReport {
            bundle_id: bundle::Id {
                source: "ipn:1.0".parse().unwrap(),
                timestamp: creation_timestamp::CreationTimestamp::now(),
                fragment_info: None,
            },
            reason: ReasonCode::NoAdditionalInformation,
            ..Default::default()
        };
        *match position {
            0 => &mut report.received,
            1 => &mut report.forwarded,
            2 => &mut report.delivered,
            _ => &mut report.deleted,
        } = Some(StatusAssertion(None));

        let decoded = roundtrip_report(&report);
        let decoded_positions = [
            decoded.received.is_some(),
            decoded.forwarded.is_some(),
            decoded.delivered.is_some(),
            decoded.deleted.is_some(),
        ];
        for (i, set) in decoded_positions.into_iter().enumerate() {
            assert_eq!(
                set,
                i == position,
                "assertion set at position {position} decoded as {decoded_positions:?}"
            );
        }
    }
}

#[test]
fn reason_code_roundtrip() {
    // Test all defined reason codes
    let codes = [
        ReasonCode::NoAdditionalInformation,
        ReasonCode::LifetimeExpired,
        ReasonCode::ForwardedOverUnidirectionalLink,
        ReasonCode::TransmissionCanceled,
        ReasonCode::DepletedStorage,
        ReasonCode::DestinationEndpointIDUnavailable,
        ReasonCode::NoKnownRouteToDestinationFromHere,
        ReasonCode::NoTimelyContactWithNextNodeOnRoute,
        ReasonCode::BlockUnintelligible,
        ReasonCode::HopLimitExceeded,
        ReasonCode::TrafficPared,
        ReasonCode::BlockUnsupported,
        ReasonCode::MissingSecurityOperation,
        ReasonCode::UnknownSecurityOperation,
        ReasonCode::UnexpectedSecurityOperation,
        ReasonCode::FailedSecurityOperation,
        ReasonCode::ConflictingSecurityOperation,
        ReasonCode::Unassigned(42),
    ];

    for code in codes {
        let v: u64 = code.into();
        let decoded = ReasonCode::try_from(v).expect("Should decode reason code");
        assert_eq!(decoded, code);
    }

    // Reserved code 255 should be rejected
    assert!(matches!(
        ReasonCode::try_from(255u64),
        Err(Error::ReservedStatusReportReason)
    ));
}

#[test]
fn reason_code_cbor_roundtrip() {
    let code = ReasonCode::HopLimitExceeded;
    let encoded = hardy_cbor::encode::emit(&code);
    let (decoded, _, _) =
        ReasonCode::from_cbor(&encoded.0).expect("Should decode reason code from CBOR");
    assert_eq!(decoded, code);
}

#[test]
fn unknown_admin_record_type() {
    // Encode an admin record with type code 99 (unknown)
    let data = hardy_cbor::encode::emit_array(Some(2), |a| {
        a.emit(&99u64);
        a.emit(&0u64);
    });
    assert!(matches!(
        AdministrativeRecord::from_cbor(&data),
        Err(Error::UnknownAdminRecordType(99))
    ));
}

// The status flag routes through `require_canonical` because a bare `bool`
// decode folds tag presence into the (discarded) canonical flag, silently
// accepting a tagged status. Pin the rejection so it cannot silently
// regress to the bare decode.
#[test]
fn tagged_status_flag_is_rejected_as_not_canonical() {
    let report = BundleStatusReport {
        bundle_id: bundle::Id {
            source: "ipn:1.0".parse().unwrap(),
            timestamp: creation_timestamp::CreationTimestamp::now(),
            fragment_info: None,
        },
        reason: ReasonCode::NoAdditionalInformation,
        ..Default::default()
    };
    let encoded = hardy_cbor::encode::emit(&report);
    // [4-array [4-array [1-array false ... — the received-status flag is the
    // fourth byte.
    assert_eq!(&encoded.0[..4], &[0x84, 0x84, 0x81, 0xF4]);

    // Tag the flag: `[false]` becomes `[#6.0(false)]`.
    let mut evil = encoded.0.to_vec();
    evil.insert(3, 0xC0);

    let Err(Error::InvalidField {
        field: "bundle status information",
        source,
    }) = BundleStatusReport::from_cbor(&evil)
    else {
        panic!("a tagged status flag must fail the status-information parse");
    };
    let Some(Error::InvalidField {
        field: "received status",
        source,
    }) = source.downcast_ref::<Error>()
    else {
        panic!("expected the error to name the received status, got {source:?}");
    };
    let Some(Error::InvalidField {
        field: "status",
        source,
    }) = source.downcast_ref::<Error>()
    else {
        panic!("expected the inner error to name the status flag, got {source:?}");
    };
    assert!(
        matches!(source.downcast_ref::<Error>(), Some(Error::NotCanonical)),
        "expected NotCanonical, got {source:?}"
    );
}
