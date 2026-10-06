#![cfg(feature = "server")]

mod common;

#[cfg(feature = "client")]
use std::borrow::Cow;
use std::{net::SocketAddr, sync::Arc, time::Duration};

use hardy_async::TaskPool;
#[cfg(feature = "client")]
use hardy_async::sync::spin::Once;
use hardy_bpa::{Bytes, bpa::Bpa};
#[cfg(feature = "client")]
use hardy_bpa::{
    async_trait,
    services::{self, ServiceSink},
    stream::{Receiver, Segment, concat_stream},
};
#[cfg(feature = "client")]
use hardy_bpv7::{
    builder::Builder,
    bundle::{self, Id as BundleId},
    creation_timestamp::CreationTimestamp,
    eid::{Eid, Service},
    status_report,
};
#[cfg(feature = "client")]
use hardy_proto::{
    chunking::DEFAULT_CHUNK_SIZE,
    client::BpaClient,
    server::{Limits, ServiceServiceImpl},
    service::{
        Delivery, ReceiveMetadata, ReceiveRequest, Register, SendMetadata, SendRequest,
        SendResponse, SubscribeRequest, SubscribeResponse, Unregister, receive_request,
        receive_response, register, send_request, service_service_client::ServiceServiceClient,
        service_service_server::ServiceServiceServer, subscribe_request, subscribe_response,
    },
};
#[cfg(feature = "client")]
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::{iter, wrappers::ReceiverStream};
use tonic::{
    Code, Status, Streaming,
    transport::{Channel, Server},
};

use common::{UnregisterWatch, build_bpa, build_bundle, ipn1, serve, timeout, wait_unregistered};

struct Harness {
    bpa: Arc<Bpa>,
    client: ServiceServiceClient<Channel>,
    #[cfg_attr(
        not(feature = "client"),
        expect(dead_code, reason = "read by the client SDK test")
    )]
    address: SocketAddr,
    watch: Arc<UnregisterWatch>,
}

async fn harness() -> Harness {
    harness_with_limits(Limits::default()).await
}

async fn harness_with_limits(limits: Limits) -> Harness {
    let bpa = build_bpa(ipn1(), true).await;

    let tasks = TaskPool::new();
    let watch = UnregisterWatch::new(bpa.clone());
    let server = ServiceServiceImpl::with_limits(watch.clone(), tasks.clone(), limits);
    let service = ServiceServiceServer::new(server.clone());
    let address = serve(Server::builder().add_service(service)).await;

    let client = ServiceServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    Harness {
        bpa,
        client,
        address,
        watch,
    }
}

struct Registered {
    requests_tx: mpsc::Sender<SubscribeRequest>,
    events: Streaming<SubscribeResponse>,
    endpoint_id: String,
    token: Bytes,
    max_chunk_size: u64,
}

async fn register(
    client: &mut ServiceServiceClient<Channel>,
    service_id: Option<register::ServiceId>,
) -> Registered {
    register_with_chunk_size(client, service_id, None).await
}

async fn register_with_chunk_size(
    client: &mut ServiceServiceClient<Channel>,
    service_id: Option<register::ServiceId>,
    max_chunk_size: Option<u64>,
) -> Registered {
    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                service_id,
                max_chunk_size,
            })),
        })
        .await
        .unwrap();

    let mut events = client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();
    let event = timeout(events.message()).await.unwrap().unwrap();
    let Some(subscribe_response::Event::Registration(registration)) = event.event else {
        panic!("expected the Registration event first");
    };
    assert!(!registration.session_token.is_empty());

    Registered {
        requests_tx,
        events,
        endpoint_id: registration.endpoint_id,
        token: registration.session_token,
        max_chunk_size: registration
            .sizes
            .expect("a Registration announces its sizes")
            .max_chunk_size,
    }
}

async fn send(
    client: &mut ServiceServiceClient<Channel>,
    token: Bytes,
    bundle: Bytes,
) -> Result<SendResponse, Status> {
    let mut messages = vec![SendRequest {
        request: Some(send_request::Request::Metadata(SendMetadata {
            session_token: token,
            bundle_size: None,
        })),
    }];
    for chunk in bundle.chunks(DEFAULT_CHUNK_SIZE) {
        messages.push(SendRequest {
            request: Some(send_request::Request::Chunk(Bytes::copy_from_slice(chunk))),
        });
    }
    messages.push(SendRequest {
        request: Some(send_request::Request::LastChunk(Bytes::new())),
    });
    client
        .send(iter(messages))
        .await
        .map(|response| response.into_inner())
}

async fn collect(
    client: &mut ServiceServiceClient<Channel>,
    token: Bytes,
    bundle_id: &str,
    abandon: bool,
) -> Result<Vec<u8>, Status> {
    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(ReceiveRequest {
            request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                session_token: token,
                bundle_id: bundle_id.to_string(),
            })),
        })
        .await
        .unwrap();

    let mut stream = client
        .receive(ReceiverStream::new(requests_rx))
        .await?
        .into_inner();
    let mut collected = Vec::new();
    let mut cancelled = false;
    loop {
        match timeout(stream.message()).await?.and_then(|r| r.response) {
            Some(receive_response::Response::Chunk(chunk)) => {
                collected.extend_from_slice(&chunk);
                if abandon && !cancelled {
                    cancelled = true;
                    let _ = requests_tx
                        .send(ReceiveRequest {
                            request: Some(receive_request::Request::Cancel(())),
                        })
                        .await;
                }
            }
            Some(receive_response::Response::LastChunk(chunk)) => {
                collected.extend_from_slice(&chunk);
                if abandon {
                    // A one-chunk transfer arrives as its last chunk alone,
                    // so the cancel is sent here if it has not been yet.
                    if !cancelled {
                        cancelled = true;
                        let _ = requests_tx
                            .send(ReceiveRequest {
                                request: Some(receive_request::Request::Cancel(())),
                            })
                            .await;
                    }
                    continue;
                }
                let _ = requests_tx
                    .send(ReceiveRequest {
                        request: Some(receive_request::Request::Ack(())),
                    })
                    .await;
                while timeout(stream.message()).await?.is_some() {}
                return Ok(collected);
            }
            None if abandon => return Ok(collected),
            other => panic!("expected a chunk, got {other:?}"),
        }
    }
}

async fn drain_undelivered(events: &mut Streaming<SubscribeResponse>, why: &str) {
    loop {
        match timeout(events.message()).await {
            Ok(Some(event)) => assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "{why}"
            ),
            Ok(None) => panic!("an unsolicited ending must state a status"),
            Err(status) => {
                assert_eq!(status.code(), Code::Unavailable);
                break;
            }
        }
    }
}

async fn delivery(registered: &mut Registered, bundle_size: u64) -> Delivery {
    loop {
        let event = timeout(registered.events.message()).await.unwrap().unwrap();
        match event.event {
            Some(subscribe_response::Event::Delivery(delivery)) => {
                assert_eq!(delivery.bundle_size, bundle_size);
                return delivery;
            }
            Some(subscribe_response::Event::BundleStatusReport(_)) => {}
            other => panic!("expected a Delivery, got {other:?}"),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_and_dynamic_registrations_mint_distinct_sessions() {
    let mut harness = harness().await;

    let explicit = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    assert_eq!(explicit.endpoint_id, "ipn:1.7");

    let dynamic = register(&mut harness.client, None).await;
    assert!(
        dynamic.endpoint_id.starts_with("ipn:1."),
        "a dynamic registration is a service on this node, got {}",
        dynamic.endpoint_id
    );
    assert_ne!(dynamic.endpoint_id, explicit.endpoint_id);
    assert_ne!(dynamic.token, explicit.token);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_to_self_roundtrip() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let bundle = build_bundle(
        &registered.endpoint_id,
        &registered.endpoint_id,
        b"a whole bundle over the v1 wire",
    );
    let sent = send(
        &mut harness.client,
        registered.token.clone(),
        bundle.clone(),
    )
    .await
    .unwrap();
    assert!(!sent.bundle_id.is_empty());

    let delivery = delivery(&mut registered, bundle.len() as u64).await;

    let collected = collect(
        &mut harness.client,
        registered.token.clone(),
        &delivery.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert_eq!(collected, bundle);

    assert_eq!(delivery.bundle_id, sent.bundle_id);
    let gone = collect(
        &mut harness.client,
        registered.token.clone(),
        &delivery.bundle_id,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(gone.code(), Code::NotFound);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncollected_delivery_expires_its_claim_and_ends_the_session() {
    let mut harness = harness_with_limits(Limits {
        claim: Duration::ZERO,
        ..Limits::default()
    })
    .await;
    let mut unregistered = harness.watch.subscribe();
    let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let bundle = build_bundle(
        &registered.endpoint_id,
        &registered.endpoint_id,
        b"never collected",
    );
    let size = bundle.len() as u64;
    send(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap();

    delivery(&mut registered, size).await;

    wait_unregistered(&mut unregistered).await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_negotiated_chunk_size_is_announced_and_held_to() {
    let mut harness = harness().await;
    let registered = register_with_chunk_size(
        &mut harness.client,
        Some(register::ServiceId::Ipn(7)),
        Some(4096),
    )
    .await;

    assert_eq!(registered.max_chunk_size, 4096);

    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: registered.token.clone(),
                bundle_size: None,
            })),
        },
        SendRequest {
            request: Some(send_request::Request::Chunk(Bytes::from(vec![0x5a; 4097]))),
        },
    ];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert_eq!(
        status.message(),
        "a chunk of 4097 bytes exceeds the agreed 4096 bytes"
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bundle_that_misses_its_declared_size_is_refused() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"declared");
    let size = bundle.len() as u64;

    for (declared, expected) in [
        (
            size - 1,
            format!("the transfer declared {} bytes, more arrived", size - 1),
        ),
        (
            size + 1,
            format!(
                "the transfer declared {} bytes, only {size} arrived",
                size + 1
            ),
        ),
    ] {
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: registered.token.clone(),
                    bundle_size: Some(declared),
                })),
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(bundle.clone())),
            },
        ];
        let status = harness.client.send(iter(messages)).await.unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), expected);
    }

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bundle_that_matches_its_declared_size_is_accepted() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"declared");

    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: registered.token.clone(),
                bundle_size: Some(bundle.len() as u64),
            })),
        },
        SendRequest {
            request: Some(send_request::Request::LastChunk(bundle)),
        },
    ];

    let sent = harness
        .client
        .send(iter(messages))
        .await
        .unwrap()
        .into_inner();
    assert!(!sent.bundle_id.is_empty());

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_truncated_send_never_commits() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, b"cut");
    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: registered.token.clone(),
                bundle_size: None,
            })),
        },
        SendRequest {
            request: Some(send_request::Request::Chunk(bundle)),
        },
    ];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::Aborted);

    harness.bpa.shutdown().await;
    drain_undelivered(&mut registered.events, "a truncated send must not deliver").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_send_is_discarded() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, b"undo");
    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: registered.token.clone(),
                bundle_size: None,
            })),
        },
        SendRequest {
            request: Some(send_request::Request::Chunk(bundle)),
        },
        SendRequest {
            request: Some(send_request::Request::Cancel(())),
        },
    ];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::Cancelled);

    harness.bpa.shutdown().await;
    drain_undelivered(&mut registered.events, "a cancelled send must not deliver").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_invalid_bundle_is_rejected() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let status = send(
        &mut harness.client,
        registered.token.clone(),
        Bytes::from_static(b"not a bundle"),
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_collection_defers_to_the_next_registration() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let payload = vec![0x5a; DEFAULT_CHUNK_SIZE + 3];
    let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, &payload);
    send(
        &mut harness.client,
        registered.token.clone(),
        bundle.clone(),
    )
    .await
    .unwrap();
    let first = delivery(&mut registered, bundle.len() as u64).await;

    collect(
        &mut harness.client,
        registered.token.clone(),
        &first.bundle_id,
        true,
    )
    .await
    .expect("a cancelled collection ends cleanly");

    let spent = collect(
        &mut harness.client,
        registered.token.clone(),
        &first.bundle_id,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(spent.code(), Code::NotFound);

    registered
        .requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(
        timeout(registered.events.message())
            .await
            .unwrap()
            .is_none()
    );

    let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let announced = delivery(&mut registered, bundle.len() as u64).await;
    let collected = collect(
        &mut harness.client,
        registered.token.clone(),
        &announced.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert_eq!(collected, bundle);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_token_is_rejected() {
    let mut harness = harness().await;
    register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"denied");
    let status = send(&mut harness.client, Bytes::from_static(b"forged"), bundle)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_source_is_rejected() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let bundle = build_bundle("ipn:1.99", "ipn:1.7", b"forged source");
    let status = send(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "the bundle's source is not the registered endpoint"
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_stream_tears_the_session_down() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"stale");
    let mut unregistered = harness.watch.subscribe();
    drop(registered.events);
    drop(registered.requests_tx);
    wait_unregistered(&mut unregistered).await;

    let status = send(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
struct SdkService {
    sink: Once<Box<dyn ServiceSink>>,
    delivered: mpsc::Sender<Bytes>,
    statuses: mpsc::Sender<(BundleId, services::StatusNotify)>,
}

#[cfg(feature = "client")]
#[async_trait]
impl services::Service for SdkService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn ServiceSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &BundleId,
        _expiry: OffsetDateTime,
        _bundle_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let data = concat_stream(stream, usize::MAX, None).await?;
        let _ = self.delivered.send(data).await;
        Ok(())
    }

    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        _from: &Eid,
        kind: services::StatusNotify,
        _reason: status_report::ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
        let _ = self.statuses.send((bundle_id.clone(), kind)).await;
    }
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_sdk_roundtrip() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
    let (statuses_tx, _statuses_rx) = mpsc::channel(4);
    let svc = Arc::new(SdkService {
        sink: Once::new(),
        delivered: delivered_tx,
        statuses: statuses_tx,
    });
    let handle = client
        .register_service(Service::Ipn(9), svc.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
    assert_eq!(eid.to_string(), "ipn:1.9");

    let bundle = build_bundle("ipn:1.9", "ipn:1.9", b"through the sdk as a whole bundle");
    let sink = svc.sink.get().unwrap();
    sink.send(&mut bundle.clone()).await.unwrap();

    let data = timeout(delivered_rx.recv()).await.unwrap();
    assert_eq!(data, bundle);

    sink.unregister().await;
    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_report_reaches_the_sending_service() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
    let (statuses_tx, mut statuses_rx) = mpsc::channel(4);
    let svc = Arc::new(SdkService {
        sink: Once::new(),
        delivered: delivered_tx,
        statuses: statuses_tx,
    });
    let _handle = client
        .register_service(Service::Ipn(9), svc.clone())
        .await
        .unwrap();

    let (built, data) = Builder::new("ipn:1.9".parse().unwrap(), "ipn:1.9".parse().unwrap())
        .with_flags(bundle::Flags {
            delivery_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:1.0".parse().unwrap())
        .with_payload(Cow::Borrowed(b"report me"))
        .build(CreationTimestamp::now())
        .unwrap();

    let sink = svc.sink.get().unwrap();
    let sent = sink.send(&mut Bytes::from(data)).await.unwrap();
    assert_eq!(sent, built.primary.id);

    let _ = timeout(delivered_rx.recv()).await.unwrap();

    let (reported, kind) = timeout(statuses_rx.recv()).await.unwrap();
    assert_eq!(reported, sent);
    assert_eq!(kind, services::StatusNotify::Delivered);

    sink.unregister().await;
    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregister_ends_the_session_and_invalidates_the_token() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    registered
        .requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(
        timeout(registered.events.message())
            .await
            .unwrap()
            .is_none(),
        "unregister must end the session stream"
    );

    let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"stale");
    let status = send(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}
