use std::sync::{Arc, Mutex};

use hardy_bpa::async_trait;
use hardy_bpv7::{
    builder::Builder, bundle::Flags, bundle::ParsedBundle, creation_timestamp::CreationTimestamp,
    eid::Eid,
};
use hardy_echo_service::EchoService;

use hardy_bpa::services::Service as _;

// A ServiceSink stub that records the bytes of every bundle the service
// sends.
struct RecordingSink(Arc<Mutex<Vec<Vec<u8>>>>);

#[async_trait]
impl hardy_bpa::services::ServiceSink for RecordingSink {
    async fn unregister(&self) {}

    async fn send(
        &self,
        stream: &mut dyn hardy_bpa::stream::Receiver<hardy_bpa::stream::Segment>,
    ) -> hardy_bpa::services::Result<hardy_bpv7::bundle::Id> {
        let mut buffer = Vec::new();
        loop {
            match stream
                .recv()
                .await
                .map_err(|_| hardy_bpa::services::Error::StreamCancelled)?
            {
                hardy_bpa::stream::Segment::Next(bytes) => buffer.extend_from_slice(&bytes),
                hardy_bpa::stream::Segment::Final(bytes) => {
                    buffer.extend_from_slice(&bytes);
                    break;
                }
            }
        }
        self.0.lock().unwrap().push(buffer);
        Ok(hardy_bpv7::bundle::Id::default())
    }
}

const ENDPOINT: &str = "ipn:2.1";

fn make_request(source: Eid, flags: Flags) -> Box<[u8]> {
    Builder::new(source, ENDPOINT.parse().unwrap())
        .with_flags(flags)
        .with_lifetime(core::time::Duration::from_secs(7200))
        .with_payload(b"echo-me".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap()
        .1
}

// Registers an echo service, delivers `data` to it, and returns the bytes
// of every response bundle it sent.
async fn deliver(data: &[u8]) -> Vec<Vec<u8>> {
    let service = EchoService::new();
    let sent = Arc::new(Mutex::new(Vec::new()));
    service
        .on_register(
            &ENDPOINT.parse().unwrap(),
            Box::new(RecordingSink(sent.clone())),
        )
        .await;

    service
        .on_deliver(
            &hardy_bpv7::bundle::Id::default(),
            time::OffsetDateTime::UNIX_EPOCH,
            data.len() as u64,
            &mut hardy_bpa::Bytes::copy_from_slice(data),
        )
        .await
        .unwrap();

    sent.lock().unwrap().clone()
}

// A bundle from the null endpoint has no return path: no response.
#[tokio::test]
async fn test_no_echo_null_source() {
    let request = make_request(
        Eid::Null,
        Flags {
            do_not_fragment: true,
            ..Default::default()
        },
    );
    assert!(deliver(&request).await.is_empty());
}

// An administrative record is never echoed (status-report reflection loops).
#[tokio::test]
async fn test_no_echo_admin_record() {
    let request = make_request(
        "ipn:1.1".parse().unwrap(),
        Flags {
            is_admin_record: true,
            ..Default::default()
        },
    );
    assert!(deliver(&request).await.is_empty());
}

// The response swaps source and destination, reflects the payload
// byte-for-byte, and adopts the request's lifetime.
#[tokio::test]
async fn test_response_swaps_endpoints() {
    let request = make_request("ipn:1.1".parse().unwrap(), Flags::default());

    let sent = deliver(&request).await;
    let [response] = sent.as_slice() else {
        panic!("expected exactly one response, got {}", sent.len());
    };

    let parsed = ParsedBundle::parse(response, hardy_bpv7::bpsec::no_keys).unwrap();
    assert_eq!(parsed.bundle.id.source, ENDPOINT.parse().unwrap());
    assert_eq!(parsed.bundle.destination, "ipn:1.1".parse().unwrap());
    assert_eq!(
        parsed.bundle.lifetime,
        core::time::Duration::from_secs(7200)
    );
    assert_eq!(
        parsed
            .bundle
            .blocks
            .get(&1)
            .and_then(|block| block.payload(response)),
        Some(b"echo-me".as_slice())
    );
    assert_eq!(parsed.bundle.flags, Flags::default());
}

// When the request asked for status reports, the response mirrors the
// request's report flags and directs its reports to the same report-to.
#[tokio::test]
async fn test_response_mirrors_report_flags_and_report_to() {
    let report_to: Eid = "ipn:9.9".parse().unwrap();
    let request = Builder::new("ipn:1.1".parse().unwrap(), ENDPOINT.parse().unwrap())
        .with_flags(Flags {
            delivery_report_requested: true,
            report_status_time: true,
            ..Default::default()
        })
        .with_report_to(report_to.clone())
        .with_lifetime(core::time::Duration::from_secs(7200))
        .with_payload(b"echo-me".as_slice().into())
        .build(CreationTimestamp::now())
        .unwrap()
        .1;

    let sent = deliver(&request).await;
    let [response] = sent.as_slice() else {
        panic!("expected exactly one response, got {}", sent.len());
    };

    let parsed = ParsedBundle::parse(response, hardy_bpv7::bpsec::no_keys).unwrap();
    assert_eq!(
        parsed.bundle.flags,
        Flags {
            delivery_report_requested: true,
            report_status_time: true,
            ..Default::default()
        }
    );
    assert_eq!(parsed.bundle.report_to, report_to);
}
