#![cfg(feature = "server")]

mod common;

use core::num::NonZeroUsize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};

use hardy_async::TaskPool;
#[cfg(feature = "client")]
use hardy_async::sync::spin::Once;
#[cfg(feature = "client")]
use hardy_bpa::stream::concat_stream;
use hardy_bpa::{
    Bytes, async_trait,
    bpa::{Bpa, BpaRegistration},
    cla::{self, Cla, ClaInit},
    node_ids::NodeIds,
    policy::FlowControllerFactory,
    routing::{self, RoutingAgent},
    services::{self, ApplicationSink, SendOptions, Service as BpaService},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id as BundleId,
    creation_timestamp::CreationTimestamp,
    eid::{DtnNodeId, Eid, IpnNodeId, NodeId, Service},
    status_report::ReasonCode,
};
#[cfg(feature = "client")]
use hardy_proto::{
    MAX_TRANSFER_SIZE,
    application::{
        Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse, Register, Registration,
        SendMetadata, SendRequest, SendResponse, SubscribeRequest, SubscribeResponse, Unregister,
        application_service_client::ApplicationServiceClient,
        application_service_server::ApplicationServiceServer, receive_request, receive_response,
        register, send_request, subscribe_request, subscribe_response,
    },
    chunking::DEFAULT_CHUNK_SIZE,
    client::BpaClient,
    server::{ApplicationServiceImpl, Limits, MAX_SERVER_MESSAGE_SIZE},
};
use time::OffsetDateTime;
use tokio::{
    spawn,
    sync::{Notify, mpsc},
};
use tokio_stream::{iter, wrappers::ReceiverStream};
use tonic::{
    Code, Status, Streaming,
    transport::{Channel, Server},
};

use common::{UnregisterWatch, build_bpa, ipn1, serve, timeout, wait_unregistered};

struct Harness {
    bpa: Arc<Bpa>,
    tasks: TaskPool,
    client: ApplicationServiceClient<Channel>,
    #[cfg_attr(
        not(feature = "client"),
        expect(dead_code, reason = "read by the client SDK test")
    )]
    address: SocketAddr,
    watch: Arc<UnregisterWatch>,
}

async fn harness() -> Harness {
    harness_with(ipn1()).await
}

async fn harness_with(node_ids: NodeIds) -> Harness {
    harness_with_limits(node_ids, Limits::default()).await
}

async fn harness_with_limits(node_ids: NodeIds, limits: Limits) -> Harness {
    let bpa = build_bpa(node_ids, true).await;

    let tasks = TaskPool::new();
    let watch = UnregisterWatch::new(bpa.clone());
    let server = ApplicationServiceImpl::with_limits(watch.clone(), tasks.clone(), limits);
    let service = ApplicationServiceServer::new(server.clone());
    let address = serve(Server::builder().add_service(service)).await;

    let client = ApplicationServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    Harness {
        bpa,
        tasks,
        client,
        address,
        watch,
    }
}

struct App {
    requests_tx: mpsc::Sender<SubscribeRequest>,
    events: Streaming<SubscribeResponse>,
    endpoint_id: String,
    token: Bytes,
}

async fn register(
    client: &mut ApplicationServiceClient<Channel>,
    service_id: Option<register::ServiceId>,
) -> App {
    register_with_chunk_size(client, service_id, None).await
}

async fn register_with_chunk_size(
    client: &mut ApplicationServiceClient<Channel>,
    service_id: Option<register::ServiceId>,
    max_chunk_size: Option<u64>,
) -> App {
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

    App {
        requests_tx,
        events,
        endpoint_id: registration.endpoint_id,
        token: registration.session_token,
    }
}

async fn send(
    client: &mut ApplicationServiceClient<Channel>,
    token: Bytes,
    destination: &str,
    adu: &[u8],
) -> Result<SendResponse, Status> {
    let metadata = SendRequest {
        request: Some(send_request::Request::Metadata(SendMetadata {
            session_token: token,
            destination: destination.to_string(),
            lifetime: Some(prost_types::Duration {
                seconds: 3600,
                nanos: 0,
            }),
            options: None,
            adu_size: None,
        })),
    };
    let mut messages = vec![metadata];
    messages.extend(chunked(adu));
    client
        .send(iter(messages))
        .await
        .map(|response| response.into_inner())
}

async fn collect(
    client: &mut ApplicationServiceClient<Channel>,
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
            None => panic!("expected a chunk"),
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

async fn delivery(app: &mut App, adu_size: u64) -> Delivery {
    loop {
        let event = timeout(app.events.message()).await.unwrap().unwrap();
        match event.event {
            Some(subscribe_response::Event::Delivery(delivery)) => {
                assert_eq!(delivery.adu_size, adu_size);
                return delivery;
            }
            Some(subscribe_response::Event::BundleStatusReport(_)) => {}
            other => panic!("expected a Delivery, got {other:?}"),
        }
    }
}

async fn send_chunked(
    client: &mut ApplicationServiceClient<Channel>,
    token: Bytes,
    destination: String,
    adu: &[u8],
) {
    let mut messages = vec![SendRequest {
        request: Some(send_request::Request::Metadata(SendMetadata {
            session_token: token,
            destination,
            lifetime: Some(prost_types::Duration {
                seconds: 3600,
                nanos: 0,
            }),
            options: None,
            adu_size: None,
        })),
    }];
    for chunk in adu.chunks(DEFAULT_CHUNK_SIZE) {
        messages.push(SendRequest {
            request: Some(send_request::Request::Chunk(Bytes::copy_from_slice(chunk))),
        });
    }
    messages.push(SendRequest {
        request: Some(send_request::Request::LastChunk(Bytes::new())),
    });
    client.send(iter(messages)).await.unwrap();
}

async fn take_all(
    client: &mut ApplicationServiceClient<Channel>,
    token: Bytes,
    bundle_id: &str,
) -> (
    mpsc::Sender<ReceiveRequest>,
    Streaming<ReceiveResponse>,
    Vec<u8>,
) {
    let (requests_tx, requests_rx) = mpsc::channel(2);
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
        .await
        .unwrap()
        .into_inner();
    let mut collected = Vec::new();
    loop {
        match timeout(stream.message())
            .await
            .unwrap()
            .unwrap()
            .response
            .unwrap()
        {
            receive_response::Response::Chunk(chunk) => collected.extend_from_slice(&chunk),
            receive_response::Response::LastChunk(chunk) => {
                collected.extend_from_slice(&chunk);
                break;
            }
        }
    }
    (requests_tx, stream, collected)
}

async fn terminal_status(stream: &mut Streaming<ReceiveResponse>) -> Status {
    loop {
        match timeout(stream.message()).await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("an abandonment must end with a status"),
            Err(status) => break status,
        }
    }
}

async fn clean_end(stream: &mut Streaming<ReceiveResponse>) {
    loop {
        match timeout(stream.message()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(status) => panic!("a cancel must end the call cleanly, got {status:?}"),
        }
    }
}

async fn recollected_after_reregistration(harness: &mut Harness, app: App, adu: &[u8]) {
    let App {
        requests_tx,
        mut events,
        ..
    } = app;
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(timeout(events.message()).await.unwrap().is_none());

    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let announced = delivery(&mut app, adu.len() as u64).await;
    let collected = collect(
        &mut harness.client,
        app.token.clone(),
        &announced.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert_eq!(collected, adu);
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
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let adu = b"hello over the v1 wire";
    let destination = app.endpoint_id.clone();
    let sent = send(&mut harness.client, app.token.clone(), &destination, adu)
        .await
        .unwrap();
    assert!(!sent.bundle_id.is_empty());

    let delivery = delivery(&mut app, adu.len() as u64).await;
    assert_eq!(delivery.source, app.endpoint_id);

    let collected = collect(
        &mut harness.client,
        app.token.clone(),
        &delivery.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert_eq!(collected, adu);

    assert_eq!(delivery.bundle_id, sent.bundle_id);
    let gone = collect(
        &mut harness.client,
        app.token.clone(),
        &delivery.bundle_id,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(gone.code(), Code::NotFound);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_truncated_send_never_commits() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let destination = app.endpoint_id.clone();
    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: app.token.clone(),
                destination,
                lifetime: Some(prost_types::Duration {
                    seconds: 3600,
                    nanos: 0,
                }),
                options: None,
                adu_size: None,
            })),
        },
        SendRequest {
            request: Some(send_request::Request::Chunk(Bytes::from_static(b"partial"))),
        },
    ];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::Aborted);

    harness.bpa.shutdown().await;
    drain_undelivered(&mut app.events, "a truncated send must not deliver").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_send_is_discarded() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let destination = app.endpoint_id.clone();
    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: app.token.clone(),
                destination,
                lifetime: Some(prost_types::Duration {
                    seconds: 3600,
                    nanos: 0,
                }),
                options: None,
                adu_size: None,
            })),
        },
        SendRequest {
            request: Some(send_request::Request::Chunk(Bytes::from_static(b"undo"))),
        },
        SendRequest {
            request: Some(send_request::Request::Cancel(())),
        },
    ];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::Cancelled);

    harness.bpa.shutdown().await;
    drain_undelivered(&mut app.events, "a cancelled send must not deliver").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn receive_of_a_malformed_id_is_invalid_argument() {
    let mut harness = harness().await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let status = collect(&mut harness.client, app.token.clone(), "not a key", false)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_collection_defers_to_the_next_registration() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let adu = vec![0x5a; DEFAULT_CHUNK_SIZE + 3];
    let destination = app.endpoint_id.clone();
    send(&mut harness.client, app.token.clone(), &destination, &adu)
        .await
        .unwrap();
    let first = delivery(&mut app, adu.len() as u64).await;

    collect(
        &mut harness.client,
        app.token.clone(),
        &first.bundle_id,
        true,
    )
    .await
    .expect("a cancelled collection ends cleanly");

    let spent = collect(
        &mut harness.client,
        app.token.clone(),
        &first.bundle_id,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(spent.code(), Code::NotFound);

    recollected_after_reregistration(&mut harness, app, &adu).await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_token_is_rejected() {
    let mut harness = harness().await;
    register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let status = send(
        &mut harness.client,
        Bytes::from_static(b"forged"),
        "ipn:1.7",
        b"denied",
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_stream_tears_the_session_down() {
    let mut harness = harness().await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let mut unregistered = harness.watch.subscribe();
    drop(app.events);
    drop(app.requests_tx);
    wait_unregistered(&mut unregistered).await;

    let status = send(&mut harness.client, app.token.clone(), "ipn:1.7", b"stale")
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_shutdown_tears_sessions_and_drains() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let shutdown = spawn({
        let tasks = harness.tasks.clone();
        async move { tasks.shutdown().await }
    });
    let status = timeout(app.events.message())
        .await
        .expect_err("pool shutdown must end the session stream");
    assert_eq!(status.code(), Code::Unavailable);
    timeout(shutdown).await.unwrap();

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_adu_delivers_end_to_end() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let destination = app.endpoint_id.clone();
    send(&mut harness.client, app.token.clone(), &destination, b"")
        .await
        .unwrap();

    let announced = delivery(&mut app, 0).await;
    let collected = collect(
        &mut harness.client,
        app.token.clone(),
        &announced.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert!(collected.is_empty());

    let gone = collect(
        &mut harness.client,
        app.token.clone(),
        &announced.bundle_id,
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(gone.code(), Code::NotFound);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_receive_before_the_announcement_is_not_found() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let unannounced = BundleId {
        source: app.endpoint_id.parse().unwrap(),
        timestamp: CreationTimestamp::now(),
        fragment_info: None,
    };
    let status = collect(
        &mut harness.client,
        app.token.clone(),
        &unannounced.to_key(),
        false,
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    let adu = b"raced";
    let destination = app.endpoint_id.clone();
    let sent = send(&mut harness.client, app.token.clone(), &destination, adu)
        .await
        .unwrap();

    let announced = delivery(&mut app, adu.len() as u64).await;
    assert_eq!(announced.bundle_id, sent.bundle_id);

    let collected = collect(
        &mut harness.client,
        app.token.clone(),
        &sent.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert_eq!(collected, adu);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_death_mid_receive_defers_the_delivery() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let adu = vec![0x5a; 16 * DEFAULT_CHUNK_SIZE];
    let destination = app.endpoint_id.clone();
    send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
    let announced = delivery(&mut app, adu.len() as u64).await;

    let (requests_tx, requests_rx) = mpsc::channel(2);
    requests_tx
        .send(ReceiveRequest {
            request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                session_token: app.token.clone(),
                bundle_id: announced.bundle_id.clone(),
            })),
        })
        .await
        .unwrap();
    let mut claimed = harness
        .client
        .receive(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();

    app.requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(timeout(app.events.message()).await.unwrap().is_none());

    let status = loop {
        match timeout(claimed.message()).await {
            Ok(Some(ReceiveResponse {
                response: Some(receive_response::Response::Chunk(_)),
            })) => continue,
            Ok(Some(ReceiveResponse {
                response: Some(receive_response::Response::LastChunk(_)),
            })) => panic!("a dead session's collection must not complete"),
            Ok(Some(response)) => panic!("unexpected response: {response:?}"),
            Ok(None) => panic!("a dead session must end the collection with a status"),
            Err(status) => break status,
        }
    };
    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(status.message(), "registration closed");

    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let announced = delivery(&mut app, adu.len() as u64).await;
    let collected = collect(
        &mut harness.client,
        app.token.clone(),
        &announced.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert_eq!(collected, adu);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancel_after_the_last_chunk_parks_the_delivery() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let adu = b"taken but not committed";
    let destination = app.endpoint_id.clone();
    send(&mut harness.client, app.token.clone(), &destination, adu)
        .await
        .unwrap();
    let announced = delivery(&mut app, adu.len() as u64).await;

    let (requests_tx, mut stream, collected) =
        take_all(&mut harness.client, app.token.clone(), &announced.bundle_id).await;
    assert_eq!(collected, adu);

    requests_tx
        .send(ReceiveRequest {
            request: Some(receive_request::Request::Cancel(())),
        })
        .await
        .unwrap();
    clean_end(&mut stream).await;

    recollected_after_reregistration(&mut harness, app, adu).await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_receipt_without_an_ack_parks_the_delivery() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let adu = b"taken then declined";
    let destination = app.endpoint_id.clone();
    send(&mut harness.client, app.token.clone(), &destination, adu)
        .await
        .unwrap();
    let announced = delivery(&mut app, adu.len() as u64).await;

    let (requests_tx, mut stream, collected) =
        take_all(&mut harness.client, app.token.clone(), &announced.bundle_id).await;
    assert_eq!(collected, adu);

    drop(requests_tx);
    assert_eq!(terminal_status(&mut stream).await.code(), Code::Cancelled);

    recollected_after_reregistration(&mut harness, app, adu).await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pool_shutdown_survives_a_claimed_unread_receive() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let adu = vec![0x5a; 16 * DEFAULT_CHUNK_SIZE];
    let destination = app.endpoint_id.clone();
    send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
    let announced = delivery(&mut app, adu.len() as u64).await;

    let (requests_tx, requests_rx) = mpsc::channel(2);
    requests_tx
        .send(ReceiveRequest {
            request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                session_token: app.token.clone(),
                bundle_id: announced.bundle_id.clone(),
            })),
        })
        .await
        .unwrap();
    let mut claimed = harness
        .client
        .receive(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();

    timeout(harness.tasks.shutdown()).await;

    // The call the shutdown cut short is ended with a status, through the
    // slot the collection holds for it, whatever the client left unread.
    let ending = loop {
        match timeout(claimed.message()).await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("a collection cut short by shutdown must end with a status"),
            Err(status) => break status,
        }
    };
    assert_eq!(ending.code(), Code::Unavailable);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_stops_feeding_stalls_at_the_feed_stage() {
    let mut harness = harness_with_limits(
        ipn1(),
        Limits {
            grace: Duration::ZERO,
            ..Limits::default()
        },
    )
    .await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let mut unregistered = harness.watch.subscribe();

    let (requests_tx, requests_rx) = mpsc::channel(1);
    requests_tx
        .send(SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: app.token.clone(),
                destination: app.endpoint_id.clone(),
                lifetime: Some(prost_types::Duration {
                    seconds: 3600,
                    nanos: 0,
                }),
                options: None,
                adu_size: None,
            })),
        })
        .await
        .unwrap();

    let status = timeout(harness.client.send(ReceiverStream::new(requests_rx)))
        .await
        .expect_err("a client that stops feeding must lose the transfer");
    assert_eq!(status.code(), Code::DeadlineExceeded);
    wait_unregistered(&mut unregistered).await;

    drop(requests_tx);
    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dtn_registration_needs_a_dtn_node_id() {
    let mut harness = harness().await;

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                service_id: Some(register::ServiceId::Dtn("mail".to_string())),
                max_chunk_size: None,
            })),
        })
        .await
        .unwrap();
    let status = timeout(harness.client.subscribe(ReceiverStream::new(requests_rx)))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dtn_registration_binds_the_dtn_endpoint() {
    let node_ids = NodeIds::try_from(
        [
            NodeId::Ipn(IpnNodeId {
                allocator_id: 0,
                node_number: 1,
            }),
            NodeId::Dtn(DtnNodeId {
                node_name: "node1".into(),
            }),
        ]
        .as_slice(),
    )
    .unwrap();
    let mut harness = harness_with(node_ids).await;

    let app = register(
        &mut harness.client,
        Some(register::ServiceId::Dtn("mail".to_string())),
    )
    .await;
    assert_eq!(app.endpoint_id, "dtn://node1/mail");

    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
struct SdkApp {
    sink: Once<Box<dyn ApplicationSink>>,
    delivered: mpsc::Sender<(Eid, Bytes)>,
    statuses: mpsc::Sender<(BundleId, services::StatusNotify)>,
}

#[cfg(feature = "client")]
#[async_trait]
impl services::Application for SdkApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        bundle_id: &BundleId,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let payload = concat_stream(stream, usize::MAX, None).await?;
        let _ = self
            .delivered
            .send((bundle_id.source.clone(), payload))
            .await;
        Ok(())
    }

    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        _from: &Eid,
        kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
        let _ = self.statuses.send((bundle_id.clone(), kind)).await;
    }
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sdk_reports_a_missing_node_id_as_such() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (delivered_tx, _delivered_rx) = mpsc::channel(1);
    let (statuses_tx, _statuses_rx) = mpsc::channel(1);
    let app = Arc::new(SdkApp {
        sink: Once::new(),
        delivered: delivered_tx,
        statuses: statuses_tx,
    });
    let Err(error) = client
        .register_application(Service::Dtn("mail".into()), app)
        .await
    else {
        panic!("an ipn-only node cannot register a dtn service");
    };
    assert!(
        matches!(error, services::Error::NoDtnNodeId),
        "the SDK must name the missing node id, got {error}"
    );

    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_sdk_roundtrip() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
    let (statuses_tx, _statuses_rx) = mpsc::channel(4);
    let app = Arc::new(SdkApp {
        sink: Once::new(),
        delivered: delivered_tx,
        statuses: statuses_tx,
    });
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();
    assert_eq!(eid.to_string(), "ipn:1.9");

    let adu = Bytes::from_static(b"through the sdk and back");
    let sink = app.sink.get().unwrap();
    sink.send(
        eid.clone(),
        Duration::from_secs(3600),
        None,
        None,
        &mut adu.clone(),
    )
    .await
    .unwrap();

    let (source, payload) = timeout(delivered_rx.recv()).await.unwrap();
    assert_eq!(source, eid);
    assert_eq!(payload, adu);

    sink.unregister().await;
    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
struct DecliningApp {
    sink: Once<Box<dyn ApplicationSink>>,
    declined: mpsc::Sender<Bytes>,
    unregistered: mpsc::Sender<()>,
}

#[cfg(feature = "client")]
#[async_trait]
impl services::Application for DecliningApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {
        let _ = self.unregistered.send(()).await;
    }

    async fn on_deliver(
        &self,
        _bundle_id: &BundleId,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let payload = concat_stream(stream, usize::MAX, None).await?;
        let _ = self.declined.send(payload).await;
        Err(services::Error::Internal("declined".into()))
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &BundleId,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sdk_decline_after_full_receipt_is_redelivered() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (declined_tx, mut declined_rx) = mpsc::channel(4);
    let (unregistered_tx, mut unregistered_rx) = mpsc::channel(1);
    let decliner = Arc::new(DecliningApp {
        sink: Once::new(),
        declined: declined_tx,
        unregistered: unregistered_tx,
    });
    let handle = client
        .register_application(Service::Ipn(9), decliner.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();

    let adu = Bytes::from_static(b"declined then redelivered");
    let sink = decliner.sink.get().unwrap();
    sink.send(
        eid.clone(),
        Duration::from_secs(3600),
        None,
        None,
        &mut adu.clone(),
    )
    .await
    .unwrap();

    let payload = timeout(declined_rx.recv()).await.unwrap();
    assert_eq!(payload, adu);

    sink.unregister().await;
    timeout(unregistered_rx.recv()).await.unwrap();

    let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
    let (statuses_tx, _statuses_rx) = mpsc::channel(4);
    let app = Arc::new(SdkApp {
        sink: Once::new(),
        delivered: delivered_tx,
        statuses: statuses_tx,
    });
    let _accepting = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    let (_, payload) = timeout(delivered_rx.recv()).await.unwrap();
    assert_eq!(payload, adu);

    app.sink.get().unwrap().unregister().await;
    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_delivery_report_reaches_the_sending_application() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
    let (statuses_tx, mut statuses_rx) = mpsc::channel(4);
    let app = Arc::new(SdkApp {
        sink: Once::new(),
        delivered: delivered_tx,
        statuses: statuses_tx,
    });
    let handle = client
        .register_application(Service::Ipn(9), app.clone())
        .await
        .unwrap();
    let eid = handle.id().clone();

    let sink = app.sink.get().unwrap();
    let sent = sink
        .send(
            eid.clone(),
            Duration::from_secs(3600),
            Some(SendOptions {
                notify_delivery: true,
                ..Default::default()
            }),
            None,
            &mut Bytes::from_static(b"report me"),
        )
        .await
        .unwrap();

    let _ = timeout(delivered_rx.recv()).await.unwrap();

    let (reported, kind) = timeout(statuses_rx.recv()).await.unwrap();
    assert_eq!(reported, sent);
    assert_eq!(kind, services::StatusNotify::Delivered);

    sink.unregister().await;
    harness.bpa.shutdown().await;
}

#[ignore = "the BPA serialises deliveries per service, so a held-open collection blocks the next announcement: see docs/TODO.md"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn re_registration_re_announces_many_parked_deliveries() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    const PARKED: usize = 48;
    let destination = app.endpoint_id.clone();
    for i in 0..PARKED {
        send(
            &mut harness.client,
            app.token.clone(),
            &destination,
            format!("parked {i}").as_bytes(),
        )
        .await
        .unwrap();
        let event = timeout(app.events.message()).await.unwrap().unwrap();
        assert!(
            matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
            "expected a Delivery, got {event:?}"
        );
    }

    app.requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(timeout(app.events.message()).await.unwrap().is_none());

    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let mut announced = HashSet::new();
    let mut last = String::new();
    while announced.len() < PARKED {
        let event = timeout(app.events.message()).await.unwrap().unwrap();
        let Some(subscribe_response::Event::Delivery(delivery)) = event.event else {
            panic!("expected a Delivery, got {event:?}");
        };
        last = delivery.bundle_id.clone();
        announced.insert(delivery.bundle_id);
    }
    assert_eq!(announced.len(), PARKED);

    let collected = collect(&mut harness.client, app.token.clone(), &last, false)
        .await
        .unwrap();
    assert!(collected.starts_with(b"parked "));

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregister_ends_the_session_and_invalidates_the_token() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    app.requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(
        timeout(app.events.message()).await.unwrap().is_none(),
        "unregister must end the session stream"
    );

    let status = send(&mut harness.client, app.token.clone(), "ipn:1.7", b"stale")
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

struct ParkedBpa {
    registered: mpsc::Sender<()>,
    release: Arc<Notify>,
    unregistered: mpsc::Sender<()>,
}

struct ParkedSink {
    unregistered: mpsc::Sender<()>,
}

#[async_trait]
impl ApplicationSink for ParkedSink {
    async fn unregister(&self) {
        let _ = self.unregistered.send(()).await;
    }

    async fn send(
        &self,
        _destination: Eid,
        _lifetime: Duration,
        _options: Option<SendOptions>,
        _size_hint: Option<u64>,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<BundleId> {
        unreachable!("the abandoned subscription never reaches a send")
    }
}

#[async_trait]
impl BpaRegistration for ParkedBpa {
    async fn register_application(
        &self,
        _service_id: Service,
        application: Arc<dyn services::Application>,
    ) -> services::Result<Eid> {
        application
            .on_register(
                &"ipn:1.7".parse().unwrap(),
                Box::new(ParkedSink {
                    unregistered: self.unregistered.clone(),
                }),
            )
            .await;
        self.registered.send(()).await.unwrap();
        self.release.notified().await;
        Ok("ipn:1.7".parse().unwrap())
    }

    async fn register_cla(
        &self,
        _name: String,
        _cla: Arc<dyn Cla>,
        _policy: Option<Arc<dyn FlowControllerFactory>>,
        _init: ClaInit,
    ) -> cla::Result<Vec<NodeId>> {
        unreachable!("the application API registers applications only")
    }

    async fn register_service(
        &self,
        _service_id: Service,
        _service: Arc<dyn BpaService>,
    ) -> services::Result<Eid> {
        unreachable!("the application API registers applications only")
    }

    async fn register_dynamic_service(
        &self,
        _service: Arc<dyn BpaService>,
    ) -> services::Result<Eid> {
        unreachable!("the application API registers applications only")
    }

    async fn register_dynamic_application(
        &self,
        _application: Arc<dyn services::Application>,
    ) -> services::Result<Eid> {
        unreachable!("this test registers an explicit service id")
    }

    async fn register_routing_agent(
        &self,
        _name: String,
        _agent: Arc<dyn RoutingAgent>,
    ) -> routing::Result<Vec<NodeId>> {
        unreachable!("the application API registers applications only")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_rpc_abandoned_during_registration_ends_unregistered() {
    let (registered_tx, mut registered_rx) = mpsc::channel(1);
    let (unregistered_tx, mut unregistered_rx) = mpsc::channel(1);
    let release = Arc::new(Notify::new());

    let tasks = TaskPool::new();
    let service = ApplicationServiceServer::new(ApplicationServiceImpl::new(
        Arc::new(ParkedBpa {
            registered: registered_tx,
            release: release.clone(),
            unregistered: unregistered_tx,
        }),
        tasks.clone(),
    ));
    let address = serve(Server::builder().add_service(service)).await;
    let client = ApplicationServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                service_id: Some(register::ServiceId::Ipn(7)),
                max_chunk_size: None,
            })),
        })
        .await
        .unwrap();
    let subscribe = spawn({
        let mut client = client.clone();
        async move { client.subscribe(ReceiverStream::new(requests_rx)).await }
    });

    timeout(registered_rx.recv()).await.unwrap();

    subscribe.abort();
    drop(requests_tx);

    release.notify_one();

    timeout(unregistered_rx.recv()).await.unwrap();

    tasks.shutdown().await;
}

#[tokio::test]
async fn an_uncollected_delivery_expires_its_claim_and_ends_the_session() {
    let harness = harness_with_limits(
        ipn1(),
        Limits {
            claim: Duration::ZERO,
            ..Limits::default()
        },
    )
    .await;
    let mut unregistered = harness.watch.subscribe();
    let mut client = harness.client.clone();
    let app = register(&mut client, Some(register::ServiceId::Ipn(9))).await;

    send(
        &mut client,
        app.token.clone(),
        &app.endpoint_id,
        b"never collected",
    )
    .await
    .unwrap();

    wait_unregistered(&mut unregistered).await;

    harness.bpa.shutdown().await;
    drop(harness.tasks);
}

// An in-process application that only keeps the sink it is handed, so a
// test can originate bundles without going through the API under test.
struct SinkOnly {
    sink: OnceLock<Box<dyn ApplicationSink>>,
}

#[async_trait]
impl services::Application for SinkOnly {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        let _ = self.sink.set(sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &BundleId,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _adu_size: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        Err(services::Error::StreamCancelled)
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &BundleId,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

#[tokio::test]
async fn a_delivery_whose_last_chunk_is_never_taken_stalls_and_ends_the_session() {
    let harness = harness_with_limits(
        ipn1(),
        Limits {
            idle: Duration::ZERO,
            ..Limits::default()
        },
    )
    .await;
    let mut unregistered = harness.watch.subscribe();
    let mut client = harness.client.clone();
    let mut app = register(&mut client, Some(register::ServiceId::Ipn(9))).await;

    // The ADU is originated in-process, because the zero idle bound would
    // also apply to a Send over the API under test.
    let sender = Arc::new(SinkOnly {
        sink: OnceLock::new(),
    });
    harness
        .bpa
        .register_application(Service::Ipn(8), sender.clone())
        .await
        .unwrap();
    let adu = b"collected but never acked";
    sender
        .sink
        .get()
        .unwrap()
        .send(
            app.endpoint_id.parse().unwrap(),
            Duration::from_secs(3600),
            None,
            None,
            &mut Bytes::from_static(adu),
        )
        .await
        .unwrap();
    let delivery = delivery(&mut app, adu.len() as u64).await;

    let (collected, status) =
        collect_unacked(&mut client, app.token.clone(), &delivery.bundle_id).await;
    assert_eq!(collected, adu);
    assert_eq!(status.code(), Code::DeadlineExceeded);

    wait_unregistered(&mut unregistered).await;

    harness.bpa.shutdown().await;
    drop(harness.tasks);
}

async fn collect_unacked(
    client: &mut ApplicationServiceClient<Channel>,
    token: Bytes,
    bundle_id: &str,
) -> (Vec<u8>, Status) {
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
        .await
        .unwrap()
        .into_inner();
    let mut collected = Vec::new();
    loop {
        match timeout(stream.message()).await {
            Ok(Some(response)) => match response.response {
                Some(receive_response::Response::Chunk(chunk))
                | Some(receive_response::Response::LastChunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                }
                None => panic!("expected a chunk"),
            },
            Ok(None) => panic!("the collection ended without a status"),
            Err(status) => return (collected, status),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_declared_adu_size_above_the_bound_is_rejected_preflight() {
    let mut harness = harness().await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let messages = [SendRequest {
        request: Some(send_request::Request::Metadata(SendMetadata {
            session_token: app.token.clone(),
            destination: app.endpoint_id.clone(),
            lifetime: Some(prost_types::Duration {
                seconds: 3600,
                nanos: 0,
            }),
            options: None,
            adu_size: Some(MAX_TRANSFER_SIZE + 1),
        })),
    }];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::ResourceExhausted);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_that_misses_its_declared_size_is_refused() {
    let mut harness = harness().await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let adu = Bytes::from_static(b"twenty-one bytes long");

    for (declared, expected) in [
        (
            adu.len() as u64 - 1,
            "the transfer declared 20 bytes, more arrived",
        ),
        (
            adu.len() as u64 + 1,
            "the transfer declared 22 bytes, only 21 arrived",
        ),
    ] {
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: app.token.clone(),
                    destination: app.endpoint_id.clone(),
                    lifetime: Some(prost_types::Duration {
                        seconds: 3600,
                        nanos: 0,
                    }),
                    options: None,
                    adu_size: Some(declared),
                })),
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(adu.clone())),
            },
        ];

        let status = harness.client.send(iter(messages)).await.unwrap_err();

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), expected);
    }

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_transfer_that_matches_its_declared_size_is_accepted() {
    let mut harness = harness().await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let adu = Bytes::from_static(b"twenty-one bytes long");

    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(SendMetadata {
                session_token: app.token.clone(),
                destination: app.endpoint_id.clone(),
                lifetime: Some(prost_types::Duration {
                    seconds: 3600,
                    nanos: 0,
                }),
                options: None,
                adu_size: Some(adu.len() as u64),
            })),
        },
        SendRequest {
            request: Some(send_request::Request::LastChunk(adu)),
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
async fn an_ack_before_the_final_chunk_never_commits() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let adu = vec![0x5a; 16 * DEFAULT_CHUNK_SIZE];
    let destination = app.endpoint_id.clone();
    send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
    let announced = delivery(&mut app, adu.len() as u64).await;

    let (requests_tx, requests_rx) = mpsc::channel(2);
    requests_tx
        .send(ReceiveRequest {
            request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                session_token: app.token.clone(),
                bundle_id: announced.bundle_id.clone(),
            })),
        })
        .await
        .unwrap();
    requests_tx
        .send(ReceiveRequest {
            request: Some(receive_request::Request::Ack(())),
        })
        .await
        .unwrap();
    let mut stream = harness
        .client
        .receive(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();

    let status = loop {
        match timeout(stream.message()).await {
            Ok(Some(ReceiveResponse {
                response: Some(receive_response::Response::Chunk(_)),
            })) => continue,
            Ok(Some(ReceiveResponse {
                response: Some(receive_response::Response::LastChunk(_)),
            })) => panic!("an early ack must never commit"),
            Ok(Some(response)) => panic!("unexpected response: {response:?}"),
            Ok(None) => panic!("an early ack must end the call with a status"),
            Err(status) => break status,
        }
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "ack before the last chunk");

    recollected_after_reregistration(&mut harness, app, &adu).await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_at_its_ceiling_refuses_a_further_session() {
    let mut harness = harness_with_limits(
        ipn1(),
        Limits {
            max_sessions: NonZeroUsize::new(1).unwrap(),
            ..Limits::default()
        },
    )
    .await;
    let _held = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let (requests_tx, requests_rx) = mpsc::channel(1);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                service_id: Some(register::ServiceId::Ipn(8)),
                max_chunk_size: None,
            })),
        })
        .await
        .unwrap();
    let status = harness
        .client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .expect_err("a session beyond the ceiling must be refused");
    assert_eq!(status.code(), Code::ResourceExhausted);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncollected_delivery_on_one_session_does_not_block_another() {
    let mut harness = harness().await;
    let mut stalled = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
    let mut healthy = register(&mut harness.client, Some(register::ServiceId::Ipn(8))).await;

    let parked = b"never collected";
    send(
        &mut harness.client,
        healthy.token.clone(),
        &stalled.endpoint_id.clone(),
        parked,
    )
    .await
    .unwrap();
    delivery(&mut stalled, parked.len() as u64).await;

    let adu = b"alive";
    let destination = healthy.endpoint_id.clone();
    send(
        &mut harness.client,
        healthy.token.clone(),
        &destination,
        adu,
    )
    .await
    .unwrap();
    let announced = delivery(&mut healthy, adu.len() as u64).await;
    let collected = collect(
        &mut harness.client,
        healthy.token.clone(),
        &announced.bundle_id,
        false,
    )
    .await
    .unwrap();
    assert_eq!(collected, adu);

    drop(stalled);
    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
struct EchoApp {
    sink: Once<Box<dyn ApplicationSink>>,
    peer: Eid,
}

#[cfg(feature = "client")]
#[async_trait]
impl services::Application for EchoApp {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn ApplicationSink>) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn on_deliver(
        &self,
        _bundle_id: &BundleId,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        _adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let mut payload = concat_stream(stream, usize::MAX, None).await?;
        self.sink
            .get()
            .unwrap()
            .send(
                self.peer.clone(),
                Duration::from_secs(3600),
                None,
                None,
                &mut payload,
            )
            .await?;
        Ok(())
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &BundleId,
        _from: &Eid,
        _kind: services::StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sdk_reply_from_within_a_delivery_does_not_deadlock() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (echoed_tx, mut echoed_rx) = mpsc::channel(64);
    let (statuses_tx, _statuses_rx) = mpsc::channel(4);
    let collector = Arc::new(SdkApp {
        sink: Once::new(),
        delivered: echoed_tx,
        statuses: statuses_tx,
    });
    let collector_handle = client
        .register_application(Service::Ipn(8), collector.clone())
        .await
        .unwrap();
    let collector_eid = collector_handle.id().clone();

    let echo = Arc::new(EchoApp {
        sink: Once::new(),
        peer: collector_eid.clone(),
    });
    let echo_handle = client
        .register_application(Service::Ipn(9), echo.clone())
        .await
        .unwrap();
    let echo_eid = echo_handle.id().clone();

    let count = 32;
    let seed = collector.sink.get().unwrap();
    for i in 0..count {
        seed.send(
            echo_eid.clone(),
            Duration::from_secs(3600),
            None,
            None,
            &mut Bytes::from(format!("echo {i}")),
        )
        .await
        .unwrap();
    }

    let mut received = 0;
    while received < count {
        let (source, _payload) = timeout(echoed_rx.recv()).await.unwrap();
        assert_eq!(source, echo_eid);
        received += 1;
    }

    seed.unregister().await;
    echo.sink.get().unwrap().unregister().await;
    harness.bpa.shutdown().await;
}

fn chunked(adu: &[u8]) -> Vec<SendRequest> {
    chunked_at(adu, DEFAULT_CHUNK_SIZE)
}

fn chunked_at(adu: &[u8], chunk_size: usize) -> Vec<SendRequest> {
    let mut messages: Vec<_> = adu
        .chunks(chunk_size)
        .map(|chunk| SendRequest {
            request: Some(send_request::Request::Chunk(Bytes::copy_from_slice(chunk))),
        })
        .collect();
    messages.push(SendRequest {
        request: Some(send_request::Request::LastChunk(Bytes::new())),
    });
    messages
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_registration_announces_the_sizes_the_session_runs_at() {
    let mut harness = harness().await;

    let registration = register_with(&mut harness, None).await;

    let sizes = registration.sizes.expect("a Registration announces sizes");
    assert_eq!(sizes.max_message_size, MAX_SERVER_MESSAGE_SIZE as u64);
    assert_eq!(sizes.max_chunk_size, DEFAULT_CHUNK_SIZE as u64);
    assert_eq!(sizes.max_transfer_size, MAX_TRANSFER_SIZE);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chunk_size_the_client_asks_for_is_the_one_announced() {
    let mut harness = harness().await;

    let registration = register_with(&mut harness, Some(4096)).await;

    let sizes = registration.sizes.expect("a Registration announces sizes");
    assert_eq!(sizes.max_chunk_size, 4096);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chunk_size_below_the_minimum_is_refused() {
    let mut harness = harness().await;
    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                service_id: Some(register::ServiceId::Ipn(7)),
                max_chunk_size: Some(1),
            })),
        })
        .await
        .unwrap();

    let status = harness
        .client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .expect_err("a chunk size below the minimum must be refused");

    assert_eq!(status.code(), Code::InvalidArgument);

    harness.bpa.shutdown().await;
}

// Registers service 7, asking for `max_chunk_size`, and returns the
// `Registration` that opens the session stream.
async fn register_with(harness: &mut Harness, max_chunk_size: Option<u64>) -> Registration {
    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                service_id: Some(register::ServiceId::Ipn(7)),
                max_chunk_size,
            })),
        })
        .await
        .unwrap();

    let mut events = harness
        .client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();
    let first = timeout(events.message()).await.unwrap().unwrap();

    let Some(subscribe_response::Event::Registration(registration)) = first.event else {
        panic!("expected the Registration event first");
    };
    registration
}

fn metadata(token: Bytes, destination: &str) -> SendMetadata {
    SendMetadata {
        session_token: token,
        destination: destination.to_string(),
        lifetime: Some(prost_types::Duration {
            seconds: 3600,
            nanos: 0,
        }),
        options: None,
        adu_size: None,
    }
}

async fn collect_chunks(
    client: &mut ApplicationServiceClient<Channel>,
    token: Bytes,
    bundle_id: &str,
) -> Vec<Bytes> {
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
        .await
        .unwrap()
        .into_inner();

    let mut chunks = Vec::new();
    loop {
        match timeout(stream.message())
            .await
            .unwrap()
            .unwrap()
            .response
            .unwrap()
        {
            receive_response::Response::Chunk(chunk) => chunks.push(chunk),
            receive_response::Response::LastChunk(chunk) => {
                chunks.push(chunk);
                break;
            }
        }
    }
    requests_tx
        .send(ReceiveRequest {
            request: Some(receive_request::Request::Ack(())),
        })
        .await
        .unwrap();
    clean_end(&mut stream).await;
    chunks
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_chunk_above_the_negotiated_size_is_refused() {
    let mut harness = harness().await;
    let app = register_with_chunk_size(
        &mut harness.client,
        Some(register::ServiceId::Ipn(7)),
        Some(4096),
    )
    .await;

    let messages = [
        SendRequest {
            request: Some(send_request::Request::Metadata(metadata(
                app.token.clone(),
                &app.endpoint_id,
            ))),
        },
        SendRequest {
            request: Some(send_request::Request::Chunk(Bytes::from(vec![0x5a; 4097]))),
        },
        SendRequest {
            request: Some(send_request::Request::LastChunk(Bytes::new())),
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
async fn a_receive_is_chunked_at_the_negotiated_size() {
    let mut harness = harness().await;
    let mut app = register_with_chunk_size(
        &mut harness.client,
        Some(register::ServiceId::Ipn(7)),
        Some(4096),
    )
    .await;

    let adu = vec![0x5a; 3 * 4096 + 5];
    let mut messages = vec![SendRequest {
        request: Some(send_request::Request::Metadata(metadata(
            app.token.clone(),
            &app.endpoint_id,
        ))),
    }];
    messages.extend(chunked_at(&adu, 4096));
    harness.client.send(iter(messages)).await.unwrap();
    let announced = delivery(&mut app, adu.len() as u64).await;

    let chunks = collect_chunks(&mut harness.client, app.token.clone(), &announced.bundle_id).await;
    assert!(
        chunks.len() > 1,
        "an ADU above the chunk size must arrive in more than one chunk"
    );
    assert!(
        chunks.iter().all(|chunk| chunk.len() <= 4096),
        "every chunk must be at most the negotiated size, got {:?}",
        chunks.iter().map(Bytes::len).collect::<Vec<_>>()
    );
    assert_eq!(chunks.concat(), adu);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscribe_that_never_registers_times_out_the_handshake() {
    let mut harness = harness_with_limits(
        ipn1(),
        Limits {
            handshake: Duration::ZERO,
            ..Limits::default()
        },
    )
    .await;

    let (requests_tx, requests_rx) = mpsc::channel::<SubscribeRequest>(1);
    let status = timeout(harness.client.subscribe(ReceiverStream::new(requests_rx)))
        .await
        .expect_err("a subscribe that sends nothing must time out");
    assert_eq!(status.code(), Code::DeadlineExceeded);
    assert_eq!(status.message(), "timed out waiting for Register");

    drop(requests_tx);
    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_that_never_sends_metadata_times_out_the_handshake() {
    let mut harness = harness_with_limits(
        ipn1(),
        Limits {
            handshake: Duration::ZERO,
            ..Limits::default()
        },
    )
    .await;

    let (requests_tx, requests_rx) = mpsc::channel::<SendRequest>(1);
    let status = timeout(harness.client.send(ReceiverStream::new(requests_rx)))
        .await
        .expect_err("a send that sends nothing must time out");
    assert_eq!(status.code(), Code::DeadlineExceeded);
    assert_eq!(status.message(), "timed out waiting for SendMetadata");

    drop(requests_tx);
    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_opened_with_a_chunk_is_invalid_argument() {
    let mut harness = harness().await;
    let _app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let messages = [SendRequest {
        request: Some(send_request::Request::Chunk(Bytes::from_static(
            b"no metadata",
        ))),
    }];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "the first message must be SendMetadata");

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscribe_opened_with_unregister_is_invalid_argument() {
    let mut harness = harness().await;

    let messages = [SubscribeRequest {
        request: Some(subscribe_request::Request::Unregister(Unregister {})),
    }];
    let status = harness.client.subscribe(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "the first message must be Register");

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_without_a_lifetime_is_invalid_argument() {
    let mut harness = harness().await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let messages = [SendRequest {
        request: Some(send_request::Request::Metadata(SendMetadata {
            lifetime: None,
            ..metadata(app.token.clone(), &app.endpoint_id)
        })),
    }];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "SendMetadata.lifetime is required");

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_to_a_destination_that_is_not_an_eid_is_invalid_argument() {
    let mut harness = harness().await;
    let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    let messages = [SendRequest {
        request: Some(send_request::Request::Metadata(metadata(
            app.token.clone(),
            "not an eid",
        ))),
    }];
    let status = harness.client.send(iter(messages)).await.unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "SendMetadata.destination is not a valid EID"
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stray_message_on_the_session_stream_is_ignored() {
    let mut harness = harness().await;
    let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

    app.requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                service_id: Some(register::ServiceId::Ipn(7)),
                max_chunk_size: None,
            })),
        })
        .await
        .unwrap();

    let adu = b"still up";
    let destination = app.endpoint_id.clone();
    let sent = send(&mut harness.client, app.token.clone(), &destination, adu)
        .await
        .unwrap();
    let announced = delivery(&mut app, adu.len() as u64).await;
    assert_eq!(announced.bundle_id, sent.bundle_id);

    // The request stream is ordered, so a clean end on Unregister proves the
    // stray Register before it was not treated as a fault.
    app.requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    loop {
        match timeout(app.events.message()).await {
            Ok(Some(_)) => {}
            Ok(None) => break,
            Err(status) => panic!("a stray message must not end the session, got {status:?}"),
        }
    }

    harness.bpa.shutdown().await;
}
