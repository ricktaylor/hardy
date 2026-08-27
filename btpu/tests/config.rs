//! Serde round trips and range validation of the configuration types.

#![cfg(feature = "serde")]

use hardy_btpu::{
    receiver::{MaxBundleSize, MaxRetainedBytes, MaxSegments, ReceiverConfig},
    sender::{BundleFraming, LinkFraming, PduSize, SendQueueDepth, SenderConfig},
    transfer::WindowSize,
};
use serde_json::{from_str, json, to_value};

#[test]
fn sender_config_round_trips_as_kebab_case_integers() {
    let config = SenderConfig {
        pdu_size: PduSize::try_from(1200).unwrap(),
        window_size: WindowSize::try_from(32).unwrap(),
        send_queue_depth: SendQueueDepth::try_from(8).unwrap(),
        link_framing: LinkFraming::Variable {
            bundle_framing: BundleFraming::Bare,
        },
    };
    let value = to_value(config).unwrap();
    assert_eq!(
        value,
        json!({
            "pdu-size": 1200,
            "window-size": 32,
            "send-queue-depth": 8,
            "link-framing": { "variable": { "bundle-framing": "bare" } },
        })
    );
    assert_eq!(
        from_str::<SenderConfig>(&value.to_string()).unwrap(),
        config
    );
}

#[test]
fn receiver_config_round_trips_as_kebab_case_integers() {
    let config = ReceiverConfig {
        window_size: WindowSize::try_from(32).unwrap(),
        max_bundle_size: MaxBundleSize::try_from(4096).unwrap(),
        max_segments_per_transfer: Some(MaxSegments::try_from(1500).unwrap()),
        max_retained_bytes: Some(MaxRetainedBytes::try_from(65_536).unwrap()),
        fec: true,
    };
    let value = to_value(config).unwrap();
    assert_eq!(
        value,
        json!({
            "window-size": 32,
            "max-bundle-size": 4096,
            "max-segments-per-transfer": 1500,
            "max-retained-bytes": 65536,
            "fec": true,
        })
    );
    assert_eq!(
        from_str::<ReceiverConfig>(&value.to_string()).unwrap(),
        config
    );
}

#[test]
fn missing_fields_take_their_defaults() {
    assert_eq!(
        from_str::<SenderConfig>("{}").unwrap(),
        SenderConfig::default()
    );
    assert_eq!(
        from_str::<ReceiverConfig>("{}").unwrap(),
        ReceiverConfig::default()
    );
    assert_eq!(ReceiverConfig::default().max_segments_per_transfer, None);
    assert_eq!(ReceiverConfig::default().max_retained_bytes, None);
    assert_eq!(
        from_str::<ReceiverConfig>(r#"{"max-segments-per-transfer": null}"#).unwrap(),
        ReceiverConfig::default()
    );
    assert_eq!(
        from_str::<SenderConfig>(r#"{"link-framing": "fixed-size"}"#).unwrap(),
        SenderConfig::default()
    );
    // The struct variant's own field defaults too.
    assert_eq!(
        from_str::<SenderConfig>(r#"{"link-framing": {"variable": {}}}"#).unwrap(),
        SenderConfig {
            link_framing: LinkFraming::Variable {
                bundle_framing: BundleFraming::Message,
            },
            ..SenderConfig::default()
        }
    );
}

#[test]
fn out_of_range_values_are_rejected_on_deserialize() {
    // The newtypes re-validate, so a configuration file cannot smuggle in
    // a value TryFrom would refuse.  serde_json wraps the newtype's own
    // error, so the message is the observable: it must be the TryFrom
    // error's text naming the range, classified as a data error.
    let cases = [
        (
            from_str::<SenderConfig>(r#"{"pdu-size": 3}"#).unwrap_err(),
            "Invalid PDU size 3 (must be 4..=1048579)",
        ),
        (
            from_str::<SenderConfig>(r#"{"window-size": 4096}"#).unwrap_err(),
            "Invalid window size 4096 (must be 4..=4095)",
        ),
        (
            from_str::<SenderConfig>(r#"{"send-queue-depth": 0}"#).unwrap_err(),
            "Invalid send queue depth 0 (must be at least 1)",
        ),
        (
            from_str::<ReceiverConfig>(r#"{"max-bundle-size": 0}"#).unwrap_err(),
            "Invalid max bundle size 0 (must be at least 1)",
        ),
        (
            from_str::<ReceiverConfig>(r#"{"max-segments-per-transfer": 0}"#).unwrap_err(),
            "Invalid max segments per transfer 0 (must be at least 1)",
        ),
        (
            from_str::<ReceiverConfig>(r#"{"max-retained-bytes": 0}"#).unwrap_err(),
            "Invalid max retained bytes 0 (must be at least 1)",
        ),
        (
            from_str::<ReceiverConfig>(r#"{"window-size": 3}"#).unwrap_err(),
            "Invalid window size 3 (must be 4..=4095)",
        ),
    ];
    for (err, expected) in cases {
        assert!(err.is_data(), "{err}");
        let text = err.to_string();
        let (message, _location) = text.split_once(" at line ").unwrap_or((&text, ""));
        assert_eq!(message, expected);
    }
}
