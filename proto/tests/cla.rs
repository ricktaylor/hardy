#![cfg(feature = "server")]

mod common;

#[cfg(feature = "client")]
use core::num::{NonZeroU32, NonZeroU64};
use std::{net::SocketAddr, sync::Arc, time::Duration};

use hardy_async::TaskPool;
#[cfg(feature = "client")]
use hardy_async::sync::spin::Once;
use hardy_bpa::{Bytes, bpa::Bpa};
#[cfg(feature = "client")]
use hardy_bpa::{
    async_trait,
    cla::{self, Cla, ForwardBundleResult},
    stream::{Receiver, Segment, concat_stream},
};
#[cfg(feature = "client")]
use hardy_bpv7::eid::NodeId;
use hardy_bpv7::{bundle::Id as BundleId, creation_timestamp::CreationTimestamp};
#[cfg(feature = "client")]
use hardy_proto::client::BpaClient;
use hardy_proto::{
    MAX_LANE_COUNT,
    chunking::DEFAULT_CHUNK_SIZE,
    cla::{
        Acceptance, AddPeerRequest, AddPeerResponse, ClaAddress, ClaAddressType, DispatchMetadata,
        DispatchRequest, DispatchResponse, ForwardMetadata, ForwardRequest, ForwardResponse,
        ForwardResult, Forwarding, Register, RemovePeerRequest, ReportTransferOutcomeRequest,
        SubscribeRequest, SubscribeResponse, Unregister, cla_service_client::ClaServiceClient,
        cla_service_server::ClaServiceServer, dispatch_request, forward_request, forward_response,
        forward_result, report_transfer_outcome_request, subscribe_request, subscribe_response,
    },
    server::{ClaServiceImpl, Limits},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    Code, Status, Streaming,
    transport::{Channel, Server},
};

use common::{UnregisterWatch, build_bpa, build_bundle, ipn1, serve, timeout, wait_unregistered};

struct Harness {
    bpa: Arc<Bpa>,
    #[expect(dead_code, reason = "held for its liveness")]
    tasks: TaskPool,
    client: ClaServiceClient<Channel>,
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
    let bpa = build_bpa(ipn1(), false).await;

    let tasks = TaskPool::new();
    let watch = UnregisterWatch::new(bpa.clone());
    let server = ClaServiceImpl::with_limits(watch.clone(), tasks.clone(), limits);
    let service = ClaServiceServer::new(server.clone());
    let address = serve(Server::builder().add_service(service)).await;

    let client = ClaServiceClient::connect(format!("http://{address}"))
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

struct Registered {
    requests_tx: mpsc::Sender<SubscribeRequest>,
    events: Streaming<SubscribeResponse>,
    node_ids: Vec<String>,
    token: Bytes,
    max_bundle_size: Option<u64>,
}

fn tcp_register(name: &str) -> Register {
    Register {
        name: name.to_string(),
        address_type: Some(ClaAddressType::Tcp.into()),
        lane_count: None,
        max_bundle_size: None,
        max_chunk_size: None,
    }
}

async fn subscribe(
    client: &mut ClaServiceClient<Channel>,
    register: Register,
) -> (
    mpsc::Sender<SubscribeRequest>,
    Result<Streaming<SubscribeResponse>, Status>,
) {
    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(register)),
        })
        .await
        .unwrap();
    let events = client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .map(|response| response.into_inner());
    (requests_tx, events)
}

async fn register(client: &mut ClaServiceClient<Channel>, name: &str) -> Registered {
    register_with(client, tcp_register(name)).await
}

async fn register_with(client: &mut ClaServiceClient<Channel>, register: Register) -> Registered {
    let (requests_tx, events) = subscribe(client, register).await;
    let mut events = events.unwrap();
    let event = timeout(events.message()).await.unwrap().unwrap();
    let Some(subscribe_response::Event::Registration(registration)) = event.event else {
        panic!("expected the Registration event first");
    };
    assert!(!registration.session_token.is_empty());

    Registered {
        requests_tx,
        events,
        node_ids: registration.node_ids,
        token: registration.session_token,
        max_bundle_size: registration.max_bundle_size,
    }
}

fn tcp_address(address: &str) -> ClaAddress {
    ClaAddress {
        address_type: ClaAddressType::Tcp.into(),
        address: Bytes::copy_from_slice(address.as_bytes()),
    }
}

async fn add_peer(
    client: &mut ClaServiceClient<Channel>,
    token: Bytes,
    node_ids: &[&str],
    address: &str,
) -> Result<AddPeerResponse, Status> {
    client
        .add_peer(AddPeerRequest {
            session_token: token,
            node_ids: node_ids.iter().map(|s| s.to_string()).collect(),
            address: Some(tcp_address(address)),
        })
        .await
        .map(|response| response.into_inner())
}

async fn dispatch(
    client: &mut ClaServiceClient<Channel>,
    token: Bytes,
    bundle: Bytes,
) -> Result<DispatchResponse, Status> {
    let mut messages = vec![DispatchRequest {
        request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
            session_token: token,
            peer_node_id: None,
            peer_address: None,
            bundle_size: None,
        })),
    }];
    for chunk in bundle.chunks(DEFAULT_CHUNK_SIZE) {
        messages.push(DispatchRequest {
            request: Some(dispatch_request::Request::Chunk(Bytes::copy_from_slice(
                chunk,
            ))),
        });
    }
    messages.push(DispatchRequest {
        request: Some(dispatch_request::Request::LastChunk(Bytes::new())),
    });
    client
        .dispatch(tokio_stream::iter(messages))
        .await
        .map(|response| response.into_inner())
}

async fn dispatch_chunked(client: &mut ClaServiceClient<Channel>, token: Bytes, bundle: &[u8]) {
    let mut messages = vec![DispatchRequest {
        request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
            session_token: token,
            peer_node_id: None,
            peer_address: None,
            bundle_size: None,
        })),
    }];
    for chunk in bundle.chunks(DEFAULT_CHUNK_SIZE) {
        messages.push(DispatchRequest {
            request: Some(dispatch_request::Request::Chunk(Bytes::copy_from_slice(
                chunk,
            ))),
        });
    }
    messages.push(DispatchRequest {
        request: Some(dispatch_request::Request::LastChunk(Bytes::new())),
    });
    client.dispatch(tokio_stream::iter(messages)).await.unwrap();
}

async fn forwarding(registered: &mut Registered) -> Forwarding {
    let event = timeout(registered.events.message()).await.unwrap().unwrap();
    let Some(subscribe_response::Event::Forwarding(forwarding)) = event.event else {
        panic!("expected a Forwarding, got {event:?}");
    };
    forwarding
}

async fn execute_forward(
    client: &mut ClaServiceClient<Channel>,
    token: Bytes,
    bundle_id: &str,
    result: forward_result::Result,
    abandon: bool,
) -> Result<Vec<u8>, Status> {
    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Metadata(ForwardMetadata {
                session_token: token,
                bundle_id: bundle_id.to_string(),
            })),
        })
        .await
        .unwrap();

    let mut stream = client
        .forward(ReceiverStream::new(requests_rx))
        .await?
        .into_inner();
    let mut collected = Vec::new();
    let mut cancelled = false;
    loop {
        match timeout(stream.message()).await?.and_then(|r| r.response) {
            Some(forward_response::Response::Chunk(chunk)) => {
                collected.extend_from_slice(&chunk);
                if abandon && !cancelled {
                    cancelled = true;
                    let _ = requests_tx
                        .send(ForwardRequest {
                            request: Some(forward_request::Request::Cancel(())),
                        })
                        .await;
                }
            }
            Some(forward_response::Response::LastChunk(chunk)) => {
                collected.extend_from_slice(&chunk);
                requests_tx
                    .send(ForwardRequest {
                        request: Some(forward_request::Request::Result(ForwardResult {
                            result: Some(result),
                        })),
                    })
                    .await
                    .unwrap();
                assert!(timeout(stream.message()).await?.is_none());
                return Ok(collected);
            }
            None if abandon => return Ok(collected),
            None => panic!("expected a chunk"),
        }
    }
}

async fn reforwarded_after_reregistration(
    harness: &mut Harness,
    registered: Registered,
    bundle_size: u64,
) {
    let Registered {
        requests_tx,
        mut events,
        ..
    } = registered;
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Unregister(Unregister {})),
        })
        .await
        .unwrap();
    assert!(timeout(events.message()).await.unwrap().is_none());

    let mut registered = register(&mut harness.client, "test-cla").await;
    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();

    let requeued = forwarding(&mut registered).await;
    assert_eq!(requeued.bundle_size, bundle_size);
    let executed = execute_forward(
        &mut harness.client,
        registered.token.clone(),
        &requeued.bundle_id,
        forward_result::Result::Sent(()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(executed.len() as u64, bundle_size);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn registration_returns_node_ids_and_a_token() {
    let mut harness = harness().await;

    let registered = register(&mut harness.client, "test-cla").await;
    assert_eq!(registered.node_ids, ["ipn:1.0"]);

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name: "test-cla".to_string(),
                address_type: None,
                lane_count: None,
                max_bundle_size: None,
                max_chunk_size: None,
            })),
        })
        .await
        .unwrap();
    let status = timeout(harness.client.subscribe(ReceiverStream::new(requests_rx)))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::AlreadyExists);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dispatch_and_forward_roundtrip() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;

    let peer = "127.0.0.1:4556";
    let added = add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        peer,
    )
    .await
    .unwrap();
    assert!(added.added);

    let payload = b"across the wire and back out";
    let bundle = build_bundle("ipn:3.1", "ipn:2.1", payload);
    let dispatched = dispatch(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap();
    assert_eq!(
        dispatched.acceptance(),
        Acceptance::Accepted,
        "a bundle the BPA took must be answered accepted, so the CLA acknowledges it"
    );

    let forwarding = forwarding(&mut registered).await;
    assert_eq!(forwarding.address, Some(tcp_address(peer)));

    let executed = execute_forward(
        &mut harness.client,
        registered.token.clone(),
        &forwarding.bundle_id,
        forward_result::Result::Sent(()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(executed.len() as u64, forwarding.bundle_size);
    assert!(executed.windows(payload.len()).any(|w| w == payload));

    harness.bpa.shutdown().await;
}

#[ignore = "the BPA parks an interrupted forward instead of redispatching it: see docs/TODO.md"]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_abandoned_forwarding_stays_queued() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;

    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();

    let payload = vec![0x5a; DEFAULT_CHUNK_SIZE + 3];
    let bundle = build_bundle("ipn:3.1", "ipn:2.1", &payload);
    dispatch(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap();
    let announced = forwarding(&mut registered).await;

    execute_forward(
        &mut harness.client,
        registered.token.clone(),
        &announced.bundle_id,
        forward_result::Result::Sent(()),
        true,
    )
    .await
    .expect("a cancelled forwarding ends cleanly");

    let requeued = forwarding(&mut registered).await;
    let executed = execute_forward(
        &mut harness.client,
        registered.token.clone(),
        &requeued.bundle_id,
        forward_result::Result::Sent(()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(executed.len() as u64, requeued.bundle_size);
    assert!(executed.windows(payload.len()).any(|w| w == payload));

    harness.bpa.shutdown().await;
}

async fn accepted_forwarding(harness: &mut Harness, registered: &mut Registered) -> Forwarding {
    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();
    let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"deferred outcome");
    dispatch(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap();
    let forwarding = forwarding(registered).await;

    execute_forward(
        &mut harness.client,
        registered.token.clone(),
        &forwarding.bundle_id,
        forward_result::Result::Accepted(()),
        false,
    )
    .await
    .unwrap();
    forwarding
}

async fn report_outcome(
    harness: &mut Harness,
    token: Bytes,
    bundle_id: &str,
    outcome: report_transfer_outcome_request::Outcome,
) {
    harness
        .client
        .report_transfer_outcome(ReportTransferOutcomeRequest {
            session_token: token,
            bundle_id: bundle_id.to_string(),
            outcome: Some(outcome),
        })
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_completed_outcome_finishes_an_accepted_forwarding() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;
    let accepted = accepted_forwarding(&mut harness, &mut registered).await;

    report_outcome(
        &mut harness,
        registered.token.clone(),
        &accepted.bundle_id,
        report_transfer_outcome_request::Outcome::Completed(()),
    )
    .await;

    // A Failed for a finished transfer has nothing left to re-queue.
    report_outcome(
        &mut harness,
        registered.token.clone(),
        &accepted.bundle_id,
        report_transfer_outcome_request::Outcome::Failed(()),
    )
    .await;

    // The peer queue offers one bundle at a time in order, so had the Failed
    // re-queued the finished bundle it would be announced before this one.
    let payload = b"behind the finished transfer";
    let bundle = build_bundle("ipn:3.1", "ipn:2.1", payload);
    dispatch(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap();
    let next = forwarding(&mut registered).await;
    assert_ne!(
        next.bundle_id, accepted.bundle_id,
        "a completed transfer must not be re-offered"
    );
    let executed = execute_forward(
        &mut harness.client,
        registered.token.clone(),
        &next.bundle_id,
        forward_result::Result::Sent(()),
        false,
    )
    .await
    .unwrap();
    assert!(executed.windows(payload.len()).any(|w| w == payload));

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_outcome_re_offers_an_accepted_forwarding() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;
    let accepted = accepted_forwarding(&mut harness, &mut registered).await;

    report_outcome(
        &mut harness,
        registered.token.clone(),
        &accepted.bundle_id,
        report_transfer_outcome_request::Outcome::Failed(()),
    )
    .await;

    let re_offered = forwarding(&mut registered).await;
    assert_eq!(re_offered.bundle_id, accepted.bundle_id);
    let executed = execute_forward(
        &mut harness.client,
        registered.token.clone(),
        &re_offered.bundle_id,
        forward_result::Result::Sent(()),
        false,
    )
    .await
    .unwrap();
    assert_eq!(executed.len() as u64, accepted.bundle_size);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_outcome_without_a_verdict_is_invalid_argument() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-cla").await;
    let bundle_id = BundleId {
        source: "ipn:3.1".parse().unwrap(),
        timestamp: CreationTimestamp::now(),
        fragment_info: None,
    };

    let status = harness
        .client
        .report_transfer_outcome(ReportTransferOutcomeRequest {
            session_token: registered.token.clone(),
            bundle_id: bundle_id.to_key(),
            outcome: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "ReportTransferOutcomeRequest.outcome is required"
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_add_peer_without_an_address_is_invalid_argument() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-cla").await;

    let status = harness
        .client
        .add_peer(AddPeerRequest {
            session_token: registered.token.clone(),
            node_ids: vec!["ipn:2.0".to_string()],
            address: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "AddPeerRequest.address is required");

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatch_naming_an_invalid_peer_node_id_is_invalid_argument() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-cla").await;

    let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"from nowhere");
    let messages = [
        DispatchRequest {
            request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                session_token: registered.token.clone(),
                peer_node_id: Some("not a node id".to_string()),
                peer_address: None,
                bundle_size: None,
            })),
        },
        DispatchRequest {
            request: Some(dispatch_request::Request::LastChunk(bundle)),
        },
    ];
    let status = harness
        .client
        .dispatch(tokio_stream::iter(messages))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "DispatchMetadata.peer_node_id is not a valid node id"
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_result_ends_the_forwarding() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;

    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();

    let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"an unanswerable forwarding");
    dispatch(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap();
    let announced = forwarding(&mut registered).await;

    let (requests_tx, requests_rx) = mpsc::channel(2);
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Metadata(ForwardMetadata {
                session_token: registered.token.clone(),
                bundle_id: announced.bundle_id.clone(),
            })),
        })
        .await
        .unwrap();
    let mut stream = harness
        .client
        .forward(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();

    loop {
        match timeout(stream.message()).await.unwrap().unwrap().response {
            Some(forward_response::Response::Chunk(_)) => {}
            Some(forward_response::Response::LastChunk(_)) => break,
            None => panic!("expected a chunk"),
        }
    }
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Result(ForwardResult {
                result: None,
            })),
        })
        .await
        .unwrap();

    let status = timeout(stream.message())
        .await
        .expect_err("an empty result must end the call");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "ForwardResult.result is required");

    reforwarded_after_reregistration(&mut harness, registered, announced.bundle_size).await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_address_type_is_refused() {
    let mut harness = harness().await;

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name: "test-cla".to_string(),
                address_type: Some(i32::MAX),
                lane_count: None,
                max_bundle_size: None,
                max_chunk_size: None,
            })),
        })
        .await
        .unwrap();

    let status = harness
        .client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    register(&mut harness.client, "test-cla").await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_uncollected_forwarding_expires_its_claim_and_ends_the_session() {
    let mut harness = harness_with_limits(Limits {
        claim: Duration::ZERO,
        ..Limits::default()
    })
    .await;
    let mut unregistered = harness.watch.subscribe();
    let mut registered = register(&mut harness.client, "test-cla").await;

    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();
    dispatch(
        &mut harness.client,
        registered.token.clone(),
        build_bundle("ipn:3.1", "ipn:2.1", b"never collected"),
    )
    .await
    .unwrap();

    forwarding(&mut registered).await;

    wait_unregistered(&mut unregistered).await;

    harness.bpa.shutdown().await;
}

async fn assert_never_forwards(registered: &mut Registered, why: &str) {
    loop {
        match timeout(registered.events.message()).await {
            Ok(Some(event)) => assert!(
                !matches!(event.event, Some(subscribe_response::Event::Forwarding(_))),
                "{why}"
            ),
            Ok(None) => return,
            Err(status) => {
                assert_eq!(status.code(), Code::Unavailable);
                return;
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_truncated_dispatch_never_commits() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;

    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();

    let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"cut short");
    let bundle = bundle.slice(..bundle.len() / 2);
    let messages = [
        DispatchRequest {
            request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                session_token: registered.token.clone(),
                peer_node_id: None,
                peer_address: None,
                bundle_size: None,
            })),
        },
        DispatchRequest {
            request: Some(dispatch_request::Request::Chunk(bundle)),
        },
    ];
    let status = harness
        .client
        .dispatch(tokio_stream::iter(messages))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Aborted);

    harness.bpa.shutdown().await;
    assert_never_forwards(&mut registered, "a truncated dispatch must not forward").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_dispatch_is_discarded() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;

    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();

    let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"undone");
    let bundle = bundle.slice(..bundle.len() / 2);
    let messages = [
        DispatchRequest {
            request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                session_token: registered.token.clone(),
                peer_node_id: None,
                peer_address: None,
                bundle_size: None,
            })),
        },
        DispatchRequest {
            request: Some(dispatch_request::Request::Chunk(bundle)),
        },
        DispatchRequest {
            request: Some(dispatch_request::Request::Cancel(())),
        },
    ];
    let status = harness
        .client
        .dispatch(tokio_stream::iter(messages))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Cancelled);

    harness.bpa.shutdown().await;
    assert_never_forwards(&mut registered, "a cancelled dispatch must not forward").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_are_added_and_removed_once() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-cla").await;

    let peer = "127.0.0.1:4556";
    assert!(
        add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            peer
        )
        .await
        .unwrap()
        .added
    );
    assert!(
        !add_peer(
            &mut harness.client,
            registered.token.clone(),
            &["ipn:2.0"],
            peer
        )
        .await
        .unwrap()
        .added
    );

    let remove = |client: &mut ClaServiceClient<Channel>, token: Bytes| {
        let request = RemovePeerRequest {
            session_token: token,
            address: Some(tcp_address(peer)),
        };
        let mut client = client.clone();
        async move { client.remove_peer(request).await }
    };
    assert!(
        remove(&mut harness.client, registered.token.clone())
            .await
            .unwrap()
            .into_inner()
            .removed
    );
    assert!(
        !remove(&mut harness.client, registered.token.clone())
            .await
            .unwrap()
            .into_inner()
            .removed
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forged_token_is_rejected() {
    let mut harness = harness().await;
    register(&mut harness.client, "test-cla").await;

    let status = add_peer(
        &mut harness.client,
        Bytes::from_static(b"forged"),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dropped_stream_tears_the_session_down() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-cla").await;

    let mut unregistered = harness.watch.subscribe();
    drop(registered.events);
    drop(registered.requests_tx);
    wait_unregistered(&mut unregistered).await;

    let status = add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name: "test-cla".to_string(),
                address_type: None,
                lane_count: None,
                max_bundle_size: None,
                max_chunk_size: None,
            })),
        })
        .await
        .unwrap();
    harness
        .client
        .subscribe(ReceiverStream::new(requests_rx))
        .await
        .unwrap();

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unregister_ends_the_session_and_invalidates_the_token() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;
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

    let status = add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap_err();
    assert_eq!(status.code(), Code::Unauthenticated);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_forward_for_a_malformed_bundle_id_is_invalid_argument() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-cla").await;

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Metadata(ForwardMetadata {
                session_token: registered.token.clone(),
                bundle_id: "no-such-bundle".to_string(),
            })),
        })
        .await
        .unwrap();
    let status = harness
        .client
        .forward(ReceiverStream::new(requests_rx))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forward_requires_the_metadata_first() {
    let mut harness = harness().await;
    let registered = register(&mut harness.client, "test-cla").await;

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Result(ForwardResult {
                result: Some(forward_result::Result::Sent(())),
            })),
        })
        .await
        .unwrap();
    let status = harness
        .client
        .forward(ReceiverStream::new(requests_rx))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    drop(registered);
    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplicate_forward_for_a_live_call_is_not_found() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;
    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();

    let payload = vec![0x5a; DEFAULT_CHUNK_SIZE + 3];
    let bundle = build_bundle("ipn:3.1", "ipn:2.1", &payload);
    dispatch(&mut harness.client, registered.token.clone(), bundle)
        .await
        .unwrap();
    let announced = forwarding(&mut registered).await;

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Metadata(ForwardMetadata {
                session_token: registered.token.clone(),
                bundle_id: announced.bundle_id.clone(),
            })),
        })
        .await
        .unwrap();
    let mut live = harness
        .client
        .forward(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();
    let first = timeout(live.message()).await.unwrap().unwrap();
    let Some(forward_response::Response::Chunk(first)) = first.response else {
        panic!("expected the first chunk, got {first:?}");
    };
    let mut collected = first.to_vec();

    let (dup_requests, dup_rx) = mpsc::channel(4);
    dup_requests
        .send(ForwardRequest {
            request: Some(forward_request::Request::Metadata(ForwardMetadata {
                session_token: registered.token.clone(),
                bundle_id: announced.bundle_id.clone(),
            })),
        })
        .await
        .unwrap();
    let status = harness
        .client
        .forward(ReceiverStream::new(dup_rx))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    loop {
        match timeout(live.message())
            .await
            .unwrap()
            .unwrap()
            .response
            .unwrap()
        {
            forward_response::Response::Chunk(chunk) => collected.extend_from_slice(&chunk),
            forward_response::Response::LastChunk(chunk) => {
                collected.extend_from_slice(&chunk);
                break;
            }
        }
    }
    assert_eq!(collected.len() as u64, announced.bundle_size);
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Result(ForwardResult {
                result: Some(forward_result::Result::Sent(())),
            })),
        })
        .await
        .unwrap();
    assert!(timeout(live.message()).await.unwrap().is_none());

    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
struct AcceptingCla {
    sink: Once<Box<dyn cla::Sink>>,
    forwarded: mpsc::Sender<BundleId>,
}

#[cfg(feature = "client")]
#[async_trait]
impl Cla for AcceptingCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        bundle_id: &BundleId,
        _total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        concat_stream(stream, usize::MAX, None)
            .await
            .map_err(|e| cla::Error::Internal(e.into()))?;
        let _ = self.forwarded.send(bundle_id.clone()).await;
        Ok(ForwardBundleResult::Accepted)
    }
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_sdk_deferred_outcome_completes_the_transfer() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (forwarded_tx, mut forwarded_rx) = mpsc::channel(4);
    let cla = Arc::new(AcceptingCla {
        sink: Once::new(),
        forwarded: forwarded_tx,
    });
    let _handle = client
        .register_cla(
            "sdk-accepting".to_string(),
            cla.clone(),
            cla::ClaInit::default(),
        )
        .await
        .unwrap();

    let sink = cla.sink.get().unwrap();
    sink.add_peer(
        cla::ClaAddress::Tcp("127.0.0.1:4556".parse().unwrap()),
        &["ipn:2.0".parse().unwrap()],
    )
    .await
    .unwrap();

    let bundle = build_bundle("ipn:3.1", "ipn:2.1", b"deferred");
    sink.dispatch(None, None, &mut bundle.clone())
        .await
        .unwrap();
    let accepted = timeout(forwarded_rx.recv()).await.unwrap();

    sink.transfer_outcome(&accepted, cla::TransferOutcome::Completed)
        .await
        .unwrap();

    harness.bpa.shutdown().await;
    drop(cla);
    timeout(async {
        assert!(
            forwarded_rx.recv().await.is_none(),
            "a completed transfer must not be re-offered"
        );
    })
    .await;
}

#[cfg(feature = "client")]
struct SdkCla {
    sink: Once<Box<dyn cla::Sink>>,
    forwarded: mpsc::Sender<Bytes>,
}

#[cfg(feature = "client")]
#[async_trait]
impl Cla for SdkCla {
    async fn on_register(
        &self,
        sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
        self.sink.call_once(|| sink);
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &BundleId,
        _total_len: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        let bundle = concat_stream(stream, usize::MAX, None)
            .await
            .map_err(|e| cla::Error::Internal(e.into()))?;
        let _ = self.forwarded.send(bundle).await;
        Ok(ForwardBundleResult::Sent)
    }
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_sdk_roundtrip() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let (forwarded_tx, mut forwarded_rx) = mpsc::channel(4);
    let cla = Arc::new(SdkCla {
        sink: Once::new(),
        forwarded: forwarded_tx,
    });
    let handle = client
        .register_cla(
            "sdk-cla".to_string(),
            cla.clone(),
            cla::ClaInit {
                address_type: Some(cla::ClaAddressType::Tcp),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!handle.id().is_empty());

    let sink = cla.sink.get().unwrap();
    let peer = cla::ClaAddress::try_from((
        cla::ClaAddressType::Tcp,
        Bytes::from_static(b"127.0.0.1:4556"),
    ))
    .unwrap();
    sink.add_peer(peer, &["ipn:2.0".parse().unwrap()])
        .await
        .unwrap();

    let payload = b"through the sdk to the link";
    let bundle = build_bundle("ipn:3.1", "ipn:2.1", payload);
    sink.dispatch(None, None, &mut bundle.clone())
        .await
        .unwrap();

    let forwarded = timeout(forwarded_rx.recv()).await.unwrap();
    assert!(forwarded.windows(payload.len()).any(|w| w == payload));

    sink.unregister().await;
    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_early_sent_never_completes_the_forwarding() {
    let mut harness = harness().await;
    let mut registered = register(&mut harness.client, "test-cla").await;

    add_peer(
        &mut harness.client,
        registered.token.clone(),
        &["ipn:2.0"],
        "127.0.0.1:4556",
    )
    .await
    .unwrap();

    let payload = vec![0x5a; 16 * DEFAULT_CHUNK_SIZE];
    let bundle = build_bundle("ipn:3.1", "ipn:2.1", &payload);
    dispatch_chunked(&mut harness.client, registered.token.clone(), &bundle).await;
    let announced = forwarding(&mut registered).await;

    let (requests_tx, requests_rx) = mpsc::channel(2);
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Metadata(ForwardMetadata {
                session_token: registered.token.clone(),
                bundle_id: announced.bundle_id.clone(),
            })),
        })
        .await
        .unwrap();
    requests_tx
        .send(ForwardRequest {
            request: Some(forward_request::Request::Result(ForwardResult {
                result: Some(forward_result::Result::Sent(())),
            })),
        })
        .await
        .unwrap();
    let mut stream = harness
        .client
        .forward(ReceiverStream::new(requests_rx))
        .await
        .unwrap()
        .into_inner();

    let status = loop {
        match timeout(stream.message()).await {
            Ok(Some(ForwardResponse {
                response: Some(forward_response::Response::Chunk(_)),
            })) => continue,
            Ok(Some(ForwardResponse {
                response: Some(forward_response::Response::LastChunk(_)),
            })) => panic!("an early result must never be given the whole bundle"),
            Ok(Some(response)) => panic!("unexpected response: {response:?}"),
            Ok(None) => panic!("an early result must end the call with a status"),
            Err(status) => break status,
        }
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(status.message(), "result before the last chunk");

    reforwarded_after_reregistration(&mut harness, registered, announced.bundle_size).await;

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lane_count_is_validated_at_registration() {
    let mut harness = harness().await;

    for lane_count in [Some(0), Some(MAX_LANE_COUNT + 1)] {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    name: "bad-lanes".to_string(),
                    address_type: None,
                    lane_count,
                    max_bundle_size: None,
                    max_chunk_size: None,
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    let (requests_tx, requests_rx) = mpsc::channel(4);
    requests_tx
        .send(SubscribeRequest {
            request: Some(subscribe_request::Request::Register(Register {
                name: "max-lanes".to_string(),
                address_type: None,
                lane_count: Some(MAX_LANE_COUNT),
                max_bundle_size: None,
                max_chunk_size: None,
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
    let event = timeout(events.message()).await.unwrap().unwrap();
    assert!(matches!(
        event.event,
        Some(subscribe_response::Event::Registration(_))
    ));

    harness.bpa.shutdown().await;
}

#[cfg(feature = "client")]
struct OverLanedCla;

#[cfg(feature = "client")]
#[async_trait]
impl Cla for OverLanedCla {
    async fn on_register(
        &self,
        _sink: Box<dyn cla::Sink>,
        _node_ids: &[NodeId],
        _max_bundle_size: Option<NonZeroU64>,
    ) {
    }

    async fn on_unregister(&self) {}

    async fn forward(
        &self,
        _lane: Option<u32>,
        _cla_addr: &cla::ClaAddress,
        _bundle_id: &BundleId,
        _total_len: u64,
        _stream: &mut dyn Receiver<Segment>,
    ) -> cla::Result<ForwardBundleResult> {
        Ok(ForwardBundleResult::Sent)
    }
}

#[cfg(feature = "client")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sdk_rejects_an_over_declared_lane_count() {
    let harness = harness().await;
    let client = BpaClient::new(format!("http://{}", harness.address), TaskPool::new()).unwrap();

    let error = client
        .register_cla(
            "over-laned".to_string(),
            Arc::new(OverLanedCla),
            cla::ClaInit {
                lane_count: NonZeroU32::new(MAX_LANE_COUNT + 1),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, cla::Error::Internal(_)),
        "expected the SDK to reject the declaration, got {error:?}"
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unspecified_address_type_is_invalid_argument() {
    let mut harness = harness().await;

    let (_requests_tx, events) = subscribe(
        &mut harness.client,
        Register {
            address_type: Some(ClaAddressType::Unspecified.into()),
            ..tcp_register("unspecified")
        },
    )
    .await;
    let Err(status) = events else {
        panic!("an unspecified address type must be refused");
    };
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        status.message(),
        "Register.address_type is unspecified or unknown"
    );

    harness.bpa.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_dispatch_declared_above_the_registration_limit_is_refused_before_any_byte() {
    let mut harness = harness().await;
    let registered = register_with(
        &mut harness.client,
        Register {
            max_bundle_size: Some(1024),
            ..tcp_register("limited")
        },
    )
    .await;
    assert_eq!(registered.max_bundle_size, Some(1024));

    // The declaration alone is sent: a server that read on would wait for
    // chunks that never come, and the test would time out rather than pass.
    let (requests_tx, requests_rx) = mpsc::channel(1);
    requests_tx
        .send(DispatchRequest {
            request: Some(dispatch_request::Request::Metadata(DispatchMetadata {
                session_token: registered.token.clone(),
                peer_node_id: None,
                peer_address: None,
                bundle_size: Some(1025),
            })),
        })
        .await
        .unwrap();
    let status = timeout(harness.client.dispatch(ReceiverStream::new(requests_rx)))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::ResourceExhausted);
    assert_eq!(
        status.message(),
        "a declared bundle of 1025 bytes exceeds the registration's limit of 1024 bytes"
    );

    drop(registered);
    harness.bpa.shutdown().await;
}
