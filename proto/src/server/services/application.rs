// The application surface: the `application.v1` wire served against
// the registration surface of a BPA. Declarations are ordered
// define-before-reference: the wire conversions, the component as the
// BPA sees it, the session's helpers, then the rpc service; within
// the service, the handlers mirror the schema's order.

use core::time::Duration;
use std::sync::Arc;

use hardy_async::TaskPool;
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    services::{self, SendOptions},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle,
    eid::{Eid, Service},
    status_report,
};
use time::OffsetDateTime;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::instrument;

use super::{Bridge, Component, Deliveries, SinkSlot, service_status, to_timestamp};
use crate::{
    MAX_TRANSFER_SIZE,
    application::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse,
        Registration, SendMetadata, SendRequest, SendResponse, StatusAssertion, SubscribeRequest,
        SubscribeResponse, application_service_server::ApplicationService, receive_request,
        register, send_request, subscribe_request, subscribe_response,
    },
    server::{
        CHANNEL_DEPTH, adapter,
        session::{Session, SessionStream},
    },
};

// One send's declared-size bound: MAX_TRANSFER_SIZE, tightened to the
// host's addressable range so an oversized transfer ends as a status
// instead of an allocation panic on 32-bit targets.
const MAX_ADU_SIZE: u64 = if MAX_TRANSFER_SIZE > isize::MAX as u64 {
    isize::MAX as u64
} else {
    MAX_TRANSFER_SIZE
};

// -------------------------------------------------------------------
// The component as the BPA sees it
// -------------------------------------------------------------------

struct GrpcApplication {
    session: Session<SubscribeResponse>,
    sink: SinkSlot<dyn services::ApplicationSink>,
    // Announced streams held for the Receive door: the single
    // collection capability per announcement.
    deliveries: Deliveries,
}

impl GrpcApplication {
    fn new(session: Session<SubscribeResponse>) -> Self {
        Self {
            session,
            sink: SinkSlot::new(),
            deliveries: Deliveries::default(),
        }
    }

    async fn event(&self, event: subscribe_response::Event) -> bool {
        self.session
            .event(SubscribeResponse { event: Some(event) })
            .await
    }
}

impl Component for GrpcApplication {
    type Event = SubscribeResponse;

    fn session(&self) -> &Session<SubscribeResponse> {
        &self.session
    }

    async fn unregister_sink(&self) {
        if let Some(sink) = self.sink.peek() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl services::Application for GrpcApplication {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.set(sink);
    }

    async fn on_unregister(&self) {
        // BPA-initiated teardown: pull the trigger; the session task
        // catches it and runs the one exit sequence.
        self.session.abort();
    }

    async fn on_deliver(
        &self,
        bundle_id: &bundle::Id,
        expiry: OffsetDateTime,
        ack_requested: bool,
        adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        let key = bundle_id.to_key();
        self.deliveries
            .serve(
                key.clone(),
                expiry,
                self.session.cancellation(),
                stream,
                || {
                    self.event(subscribe_response::Event::Delivery(Delivery {
                        bundle_id: key,
                        // The wire's source is the bundle id's source
                        // component, carried separately as a convenience.
                        source: bundle_id.source.to_string(),
                        expire_time: Some(to_timestamp(expiry)),
                        ack_requested,
                        adu_size,
                    }))
                },
            )
            .await
    }

    async fn on_status_notify(
        &self,
        bundle_id: &bundle::Id,
        from: &Eid,
        kind: services::StatusNotify,
        reason: status_report::ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        // Fire-and-forget: a torn-down session just drops the report.
        self.event(subscribe_response::Event::BundleStatusReport(
            BundleStatusReport {
                bundle_id: bundle_id.to_key(),
                reporting_node: from.to_string(),
                assertion: StatusAssertion::from(kind).into(),
                reason_code: u64::from(reason),
                status_time: timestamp.map(to_timestamp),
            },
        ))
        .await;
    }
}

// -------------------------------------------------------------------
// Control plane: the session
// -------------------------------------------------------------------

/// The application bridge. Shutting down the pool given to
/// [`new`](Self::new) tears the sessions and drives unregistration,
/// so shut it down only after the transport has stopped accepting.
#[derive(Clone)]
pub struct ApplicationServiceImpl {
    bridge: Bridge<GrpcApplication>,
}

impl ApplicationServiceImpl {
    /// Bridges the registration surface of `bpa`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            bridge: Bridge::new(bpa, tasks),
        }
    }
}

#[async_trait]
impl ApplicationService for ApplicationServiceImpl {
    type SubscribeStream = SessionStream<SubscribeResponse>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn subscribe(
        &self,
        request: Request<Streaming<SubscribeRequest>>,
    ) -> Result<Response<Self::SubscribeStream>, Status> {
        let mut requests = request.into_inner();

        // The wire requires Register first, sent without waiting for
        // response headers, so the handshake runs inline and a failed
        // registration is a plain error on the call.
        let Some(subscribe_request::Request::Register(register)) =
            requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be Register",
            ));
        };
        let service = register.service_id.map(|id| match id {
            register::ServiceId::Ipn(n) => Service::Ipn(n),
            register::ServiceId::Dtn(demux) => Service::Dtn(demux.into()),
        });

        // The token is minted before the BPA sees the component: the
        // session must be able to carry events the moment registration
        // completes. The token's `sub` prefix is the requested identity,
        // observability only; the reply carries the resolved endpoint.
        let sub = match &service {
            Some(Service::Ipn(n)) => format!("ipn:{n}"),
            Some(Service::Dtn(name)) => format!("dtn:{name}"),
            None => "dynamic".to_string(),
        };
        let token = self.bridge.sessions.mint(&sub);
        let (events_tx, events_rx) = mpsc::channel(CHANNEL_DEPTH);
        let application = Arc::new(GrpcApplication::new(Session::new(
            token.clone(),
            self.bridge.tasks.child_token(),
            events_tx,
        )));

        let endpoint_id = match service {
            Some(service) => {
                self.bridge
                    .bpa
                    .register_application(service, application.clone())
                    .await
            }
            None => {
                self.bridge
                    .bpa
                    .register_dynamic_application(application.clone())
                    .await
            }
        }
        .map_err(service_status)?;

        // The stream yields the Registration first, by construction:
        // the BPA announces parked bundles from inside `register_*`
        // itself, and those events must not outrun it.
        let registration = SubscribeResponse {
            event: Some(subscribe_response::Event::Registration(Registration {
                endpoint_id: endpoint_id.to_string(),
                session_token: token.into(),
            })),
        };
        Ok(Response::new(self.bridge.open_session(
            application,
            registration,
            events_rx,
            requests,
            "application_session",
        )))
    }

    // ---------------------------------------------------------------
    // Data plane: the doors
    // ---------------------------------------------------------------

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn send(
        &self,
        request: Request<Streaming<SendRequest>>,
    ) -> Result<Response<SendResponse>, Status> {
        let mut requests = request.into_inner();

        let Some(send_request::Request::Metadata(SendMetadata {
            session_token,
            destination,
            lifetime,
            options,
            adu_size,
        })) = requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let application = self.bridge.sessions.resolve(session_token)?;
        let cancelled = application.session.cancellation();

        let destination = destination
            .parse::<Eid>()
            .map_err(|e| Status::invalid_argument(format!("Invalid destination: {e}")))?;
        let lifetime = lifetime
            .ok_or_else(|| Status::invalid_argument("Missing lifetime"))
            .and_then(|d| {
                Duration::try_from(d)
                    .map_err(|e| Status::invalid_argument(format!("Invalid lifetime: {e}")))
            })?;

        // A declared size larger than we would ever accept is rejected up
        // front rather than streamed to the ceiling and then failed.
        if adu_size.is_some_and(|size| size > MAX_ADU_SIZE) {
            return Err(Status::resource_exhausted(
                "Declared ADU size exceeds the maximum transfer size",
            ));
        }

        // The wire's declared ADU size is the BPA's reassembly hint, so a
        // client that declared one lets the BPA pre-size.
        let options = options.map(SendOptions::from);

        // The BPA pulls the transfer chunk by chunk and assembles the
        // ADU behind its own bundle size bound (canonical CBOR needs
        // the payload's definite length before the bundle can be
        // built); nothing materialises in the bridge.
        let mut reader = adapter::Reader::new(requests, cancelled, "Send");
        match application
            .sink
            .get()?
            .send(destination, lifetime, options, adu_size, &mut reader)
            .await
        {
            Ok(bundle_id) => Ok(Response::new(SendResponse {
                bundle_id: bundle_id.to_key(),
            })),
            Err(e) => Err(reader.status().unwrap_or_else(|| service_status(e))),
        }
    }

    type ReceiveStream = ReceiverStream<Result<ReceiveResponse, Status>>;

    #[cfg_attr(feature = "instrument", instrument(skip_all))]
    async fn receive(
        &self,
        request: Request<Streaming<ReceiveRequest>>,
    ) -> Result<Response<Self::ReceiveStream>, Status> {
        let mut requests = request.into_inner();

        let Some(receive_request::Request::Metadata(ReceiveMetadata {
            session_token,
            bundle_id,
        })) = requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let application = self.bridge.sessions.resolve(session_token)?;
        let response = application
            .deliveries
            .collect(
                &self.bridge.tasks,
                application.session.cancellation(),
                &bundle_id,
                requests,
            )
            .await?;
        Ok(Response::new(response))
    }
}

// The wire against a real BPA: the generated client, a port-0
// listener, and event-driven waits.
#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    use hardy_bpa::{Bytes, bpa::Bpa, node_ids::NodeIds};
    use hardy_bpv7::eid::{IpnNodeId, NodeId};
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::{
        super::tests::{build_bpa, ipn1, serve, timeout, wait_torn_down},
        *,
    };
    use crate::application::{
        Register, Unregister, application_service_client::ApplicationServiceClient,
        application_service_server::ApplicationServiceServer, receive_response,
    };
    use crate::server::session::Sessions;

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear the sessions.
        tasks: TaskPool,
        client: ApplicationServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: std::net::SocketAddr,
        // The session index, for the teardown barrier.
        sessions: Arc<Sessions<GrpcApplication>>,
    }

    // A running BPA (node ipn:1) behind the bridge on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        harness_with(ipn1()).await
    }

    async fn harness_with(node_ids: NodeIds) -> Harness {
        // Status reports on: the report round-trip test needs them.
        let bpa = build_bpa(node_ids, true).await;

        let tasks = TaskPool::new();
        let service_impl = ApplicationServiceImpl::new(bpa.clone(), tasks.clone());
        let sessions = service_impl.bridge.sessions.clone();
        let service = ApplicationServiceServer::new(service_impl);
        let address = serve(Server::builder().add_service(service)).await;

        let client = ApplicationServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();
        Harness {
            bpa,
            tasks,
            client,
            address,
            sessions,
        }
    }

    struct App {
        requests: mpsc::Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        endpoint_id: String,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(
        client: &mut ApplicationServiceClient<Channel>,
        service_id: Option<register::ServiceId>,
    ) -> App {
        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id,
                })),
            })
            .await
            .unwrap();

        let mut events = client
            .subscribe(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();
        let event = timeout(events.message()).await.unwrap().unwrap();
        let Some(subscribe_response::Event::Registration(registration)) = event.event else {
            panic!("expected the Registration event first, got {event:?}");
        };
        assert!(!registration.session_token.is_empty());

        App {
            requests,
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
        let messages = [
            SendRequest {
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
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(Bytes::copy_from_slice(
                    adu,
                ))),
            },
        ];
        client
            .send(tokio_stream::iter(messages))
            .await
            .map(|response| response.into_inner())
    }

    // Collects one announced delivery, committing it with the in-band
    // ack; `abandon` instead answers the first chunk with an in-stream
    // cancel and surfaces the terminal status.
    async fn collect(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
        abandon: bool,
    ) -> Result<Vec<u8>, Status> {
        // Keep the request stream open for the whole collection, as the
        // SDK's Reader does: metadata first, then Ack on completion (which
        // commits) or Cancel to abandon.
        let (requests, rx) = tokio::sync::mpsc::channel(4);
        requests
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: token,
                    bundle_id: bundle_id.to_string(),
                })),
            })
            .await
            .unwrap();

        let mut stream = client
            .receive(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await?
            .into_inner();
        let mut collected = Vec::new();
        let mut cancelled = false;
        loop {
            match stream.message().await?.and_then(|r| r.response) {
                Some(receive_response::Response::Chunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if abandon && !cancelled {
                        cancelled = true;
                        let _ = requests
                            .send(ReceiveRequest {
                                request: Some(receive_request::Request::Cancel(())),
                            })
                            .await;
                    }
                }
                Some(receive_response::Response::LastChunk(chunk)) => {
                    collected.extend_from_slice(&chunk);
                    if abandon {
                        // The final chunk may already be queued when the
                        // cancel lands; only the terminal status ends an
                        // abandonment, so keep reading for it.
                        continue;
                    }
                    let _ = requests
                        .send(ReceiveRequest {
                            request: Some(receive_request::Request::Ack(())),
                        })
                        .await;
                    // Drain to EOS so the ack reaches the server (which
                    // commits, then closes the response) before the call
                    // is dropped.
                    while stream.message().await?.is_some() {}
                    return Ok(collected);
                }
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
    }

    // Awaits the Delivery announcing `adu_size` bytes on the session
    // stream.
    async fn delivery(app: &mut App, adu_size: u64) -> Delivery {
        loop {
            let event = timeout(app.events.message()).await.unwrap().unwrap();
            match event.event {
                Some(subscribe_response::Event::Delivery(delivery)) => {
                    assert_eq!(delivery.adu_size, adu_size);
                    return delivery;
                }
                // Status reports about the sent bundle may interleave.
                Some(subscribe_response::Event::BundleStatusReport(_)) => {}
                other => panic!("expected a Delivery, got {other:?}"),
            }
        }
    }

    // Sends `adu` as a chunked transfer (metadata, one chunk per
    // CHUNK_SIZE slice, an empty last_chunk), for the tests that need
    // several wire chunks in flight.
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
        for chunk in adu.chunks(crate::CHUNK_SIZE) {
            messages.push(SendRequest {
                request: Some(send_request::Request::Chunk(Bytes::copy_from_slice(chunk))),
            });
        }
        messages.push(SendRequest {
            request: Some(send_request::Request::LastChunk(Bytes::new())),
        });
        client.send(tokio_stream::iter(messages)).await.unwrap();
    }

    // Opens a Receive by hand and takes the transfer through its last
    // chunk without acknowledging, keeping the request side open: the
    // caller delivers the verdict.
    async fn take_all(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
    ) -> (
        mpsc::Sender<ReceiveRequest>,
        Streaming<ReceiveResponse>,
        Vec<u8>,
    ) {
        let (requests, rx) = mpsc::channel(2);
        requests
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: token,
                    bundle_id: bundle_id.to_string(),
                })),
            })
            .await
            .unwrap();
        let mut stream = client
            .receive(ReceiverStream::new(rx))
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
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
        (requests, stream, collected)
    }

    // Reads the response to its terminal status: an abandonment must end
    // the call with one, never a clean close.
    async fn terminal_status(stream: &mut Streaming<ReceiveResponse>) -> Status {
        loop {
            match timeout(stream.message()).await {
                Ok(Some(_)) => {}
                Ok(None) => panic!("an abandonment must end with a status"),
                Err(status) => break status,
            }
        }
    }

    // Ends `app`'s session, then asserts the parked bundle is announced
    // afresh to the next ipn:7 registration and collects (and commits)
    // whole: the deferred-not-lost tail every parking test shares.
    async fn recollected_after_reregistration(harness: &mut Harness, app: App, adu: &[u8]) {
        let App {
            requests,
            mut events,
            ..
        } = app;
        requests
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
        assert!(!dynamic.endpoint_id.is_empty());
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

        // The completed collection consumed the delivery: the sent and
        // announced ids were the same real bundle id, and it is gone.
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

        // Chunks but no last chunk: the half-close is a truncation,
        // not a commit.
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
        let status = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Aborted);

        // The BPA shutdown joins its worker pool, so anything it would
        // announce has been announced and the session stream then ends;
        // draining it to that end must surface no Delivery.
        harness.bpa.shutdown().await;
        while let Some(event) = app.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a truncated send must not deliver"
            );
        }
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
        let status = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Cancelled);

        // The BPA shutdown joins its worker pool, so anything it would
        // announce has been announced and the session stream then ends;
        // draining it to that end must surface no Delivery.
        harness.bpa.shutdown().await;
        while let Some(event) = app.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a cancelled send must not deliver"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn receive_of_an_unannounced_id_is_not_found() {
        let mut harness = harness().await;
        let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Never announced (malformed ids included): nothing is held
        // under that key for this session.
        let status = collect(&mut harness.client, app.token.clone(), "not a key", false)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::NotFound);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_abandoned_collection_defers_to_the_next_registration() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = vec![0x5a; crate::CHUNK_SIZE + 3];
        let destination = app.endpoint_id.clone();
        send(&mut harness.client, app.token.clone(), &destination, &adu)
            .await
            .unwrap();
        let first = delivery(&mut app, adu.len() as u64).await;

        // Abandoning with an in-band cancel ends the collection without
        // acknowledging it, with the abandonment status; the bundle is
        // parked, not finalized.
        let abandoned = collect(
            &mut harness.client,
            app.token.clone(),
            &first.bundle_id,
            true,
        )
        .await
        .unwrap_err();
        assert_eq!(abandoned.code(), Code::Cancelled);

        // The announced stream was the single collection capability:
        // a repeat collection in this session answers not-found.
        let spent = collect(
            &mut harness.client,
            app.token.clone(),
            &first.bundle_id,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(spent.code(), Code::NotFound);

        // Deferred, not lost.
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

        // The client vanishes without Unregister: dropping the rpc's
        // streams is caught by the response-stream guard and the request
        // half-close, and the session tears down. Subscribe to the
        // teardown signal before dropping, then wait for it, so the
        // rejection below is asserted without a race (the timeout only
        // bounds a regression).
        let mut torn = harness.sessions.torn_down();
        drop(app.events);
        drop(app.requests);
        wait_torn_down(&mut torn, &app.token).await;

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

        // The pool's token is the parent of every session trigger:
        // shutdown must end the session stream with no client action,
        // and must drain once the ended rpc releases the reader.
        let shutdown = tokio::spawn({
            let tasks = harness.tasks.clone();
            async move { tasks.shutdown().await }
        });
        assert!(
            timeout(app.events.message()).await.unwrap().is_none(),
            "pool shutdown must end the session stream"
        );
        timeout(shutdown).await.unwrap();

        harness.bpa.shutdown().await;
    }

    // An empty ADU delivers end to end: the collection is a lone empty
    // last_chunk, a completion, never a truncation.
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

        // The empty collection still completed and consumed the
        // delivery.
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

    // The declared-size pre-flight: above the bound is rejected before
    // any bytes arrive; within the bound the declaration is only a
    // hint, and an inaccurate one still commits on last_chunk.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_declared_adu_size_above_the_bound_is_rejected_preflight() {
        let mut harness = harness().await;
        let app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let metadata = |adu_size| SendMetadata {
            session_token: app.token.clone(),
            destination: app.endpoint_id.clone(),
            lifetime: Some(prost_types::Duration {
                seconds: 3600,
                nanos: 0,
            }),
            options: None,
            adu_size,
        };

        let messages = [SendRequest {
            request: Some(send_request::Request::Metadata(metadata(Some(
                MAX_ADU_SIZE + 1,
            )))),
        }];
        let status = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::ResourceExhausted);

        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(metadata(Some(1024 * 1024)))),
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(Bytes::from_static(
                    b"smaller than declared",
                ))),
            },
        ];
        let sent = harness
            .client
            .send(tokio_stream::iter(messages))
            .await
            .unwrap()
            .into_inner();
        assert!(!sent.bundle_id.is_empty());

        harness.bpa.shutdown().await;
    }

    // The announced stream is held before the Delivery event goes out,
    // so a Receive racing the announcement lands as soon as the entry
    // exists, and an early NOT_FOUND neither consumes nor poisons
    // anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_receive_racing_the_announcement_lands() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = b"raced";
        let destination = app.endpoint_id.clone();
        let sent = send(&mut harness.client, app.token.clone(), &destination, adu)
            .await
            .unwrap();

        // Poll the Receive door with the sent id without reading the
        // session stream: early attempts may answer NOT_FOUND while
        // the announcement is in flight; the first success collects
        // the whole ADU.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let collected = loop {
            match collect(
                &mut harness.client,
                app.token.clone(),
                &sent.bundle_id,
                false,
            )
            .await
            {
                Ok(collected) => break collected,
                Err(status) => {
                    assert_eq!(status.code(), Code::NotFound);
                    assert!(
                        tokio::time::Instant::now() < deadline,
                        "the announcement never landed"
                    );
                    tokio::task::yield_now().await;
                }
            }
        };
        assert_eq!(collected, adu);

        // The Delivery event still arrives on the session stream even
        // though the collection already completed.
        let announced = delivery(&mut app, adu.len() as u64).await;
        assert_eq!(announced.bundle_id, sent.bundle_id);

        harness.bpa.shutdown().await;
    }

    // A session dying while a claimed Receive is mid-stream, with the
    // final segment still unpulled, leaves the bundle parked: the next
    // registration is announced the bundle afresh and collects whole.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_death_mid_receive_defers_the_delivery() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Sixteen wire chunks of data: far more than the bridge's
        // shallow buffer plus transport windows can absorb, so with
        // the client not reading, the pump parks well before the
        // final segment.
        let adu = vec![0x5a; 16 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Claim the Receive (awaiting the call means the handler ran:
        // the stream is claimed and its first pull probed), then read
        // nothing.
        let (requests, rx) = mpsc::channel(2);
        requests
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
            .receive(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // Kill the session with the collection mid-stream.
        app.requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(timeout(app.events.message()).await.unwrap().is_none());

        // The claimed stream must end without its last chunk:
        // truncation, never completion.
        loop {
            match claimed.message().await {
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::Chunk(_)),
                })) => continue,
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::LastChunk(_)),
                })) => panic!("a dead session's collection must not complete"),
                Ok(Some(_)) | Ok(None) | Err(_) => break,
            }
        }

        // Deferred, not lost: the next registration collects it whole.
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

    // Completion is the ack, not the last chunk: a cancel arriving after
    // the final chunk, with no ack sent, abandons the collection and the
    // bundle is re-announced to the next registration.
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

        let (requests, mut stream, collected) =
            take_all(&mut harness.client, app.token.clone(), &announced.bundle_id).await;
        assert_eq!(collected, adu);

        // The last chunk is in hand but no ack was sent: the cancel is
        // the verdict, and the call ends with the abandonment status.
        requests
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Cancel(())),
            })
            .await
            .unwrap();
        assert_eq!(terminal_status(&mut stream).await.code(), Code::Cancelled);

        // Parked, not finalized.
        recollected_after_reregistration(&mut harness, app, adu).await;

        harness.bpa.shutdown().await;
    }

    // A client that takes the whole ADU and then goes silent (its
    // application declined the delivery, or it died before
    // acknowledging) commits nothing: silence never acks, and the bundle
    // is re-announced to the next registration.
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

        let (requests, mut stream, collected) =
            take_all(&mut harness.client, app.token.clone(), &announced.bundle_id).await;
        assert_eq!(collected, adu);

        // The whole ADU is in hand, but a request stream that ends
        // without an ack is an abandonment, not the inert half-close it
        // is on the session stream.
        drop(requests);
        assert_eq!(terminal_status(&mut stream).await.code(), Code::Cancelled);

        // Parked, not finalized.
        recollected_after_reregistration(&mut harness, app, adu).await;

        harness.bpa.shutdown().await;
    }

    // An ack racing the drain is a protocol violation, never a commit: a
    // conforming client cannot ack before it holds the last chunk, and a
    // misbehaving one must not be able to finalize a bundle it has not
    // received. The collection ends without its last chunk and the
    // bundle is re-announced to the next registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ack_before_the_final_chunk_never_commits() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Sixteen wire chunks of data: far more than the bridge's
        // shallow buffer plus transport windows can absorb, so the drain
        // cannot complete before the early ack (sent ahead of any read)
        // is seen.
        let adu = vec![0x5a; 16 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Ack straight after the metadata, before reading a single chunk.
        let (requests, rx) = mpsc::channel(2);
        requests
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: app.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        requests
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Ack(())),
            })
            .await
            .unwrap();
        let mut stream = harness
            .client
            .receive(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // The stream must end without its last chunk: truncation, never
        // completion. (The INVALID_ARGUMENT status is best-effort: the
        // violation fires against a full response channel, so the
        // refusal may surface as a bare truncation instead.)
        loop {
            match timeout(stream.message()).await {
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::Chunk(_)),
                })) => continue,
                Ok(Some(ReceiveResponse {
                    response: Some(receive_response::Response::LastChunk(_)),
                })) => panic!("an early ack must never commit"),
                Ok(Some(_)) | Ok(None) | Err(_) => break,
            }
        }

        // Parked, not finalized.
        recollected_after_reregistration(&mut harness, app, &adu).await;

        harness.bpa.shutdown().await;
    }

    // A client that claims a large collection and stops reading, while
    // keeping its connection alive, must not wedge pool shutdown: the
    // parked pump abandons its terminal status instead of awaiting a
    // channel nothing drains.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pool_shutdown_survives_a_claimed_unread_receive() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = vec![0x5a; 16 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Claim the collection and read nothing, keeping the call (and
        // its connection) alive across the shutdown.
        let (requests, rx) = mpsc::channel(2);
        requests
            .send(ReceiveRequest {
                request: Some(receive_request::Request::Metadata(ReceiveMetadata {
                    session_token: app.token.clone(),
                    bundle_id: announced.bundle_id.clone(),
                })),
            })
            .await
            .unwrap();
        let _claimed = harness
            .client
            .receive(ReceiverStream::new(rx))
            .await
            .unwrap()
            .into_inner();

        // Shutdown must drain despite the parked, unread pump.
        timeout(harness.tasks.shutdown()).await;

        harness.bpa.shutdown().await;
    }

    // A stalled session must not starve the pipeline: announcements
    // park on the stalled registration's own tasks, past its event
    // buffer, while other registrations keep delivering.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_session_does_not_starve_other_registrations() {
        let mut harness = harness().await;
        // Registered, then never read again: its event buffer fills.
        let stalled = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let mut healthy = register(&mut harness.client, Some(register::ServiceId::Ipn(8))).await;

        // Flood the stalled endpoint well past its session buffer.
        for i in 0..24 {
            send(
                &mut harness.client,
                healthy.token.clone(),
                &stalled.endpoint_id,
                format!("flood {i}").as_bytes(),
            )
            .await
            .unwrap();
        }

        // The healthy endpoint still delivers promptly.
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

        // Shutdown drains despite the parked announcements: session
        // teardown frees them before the dispatcher waits.
        drop(stalled);
        harness.bpa.shutdown().await;
    }

    // A dtn-scheme registration needs a dtn node id: on an ipn-only
    // node it fails the handshake with FAILED_PRECONDITION.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dtn_registration_needs_a_dtn_node_id() {
        let mut harness = harness().await;

        let (requests, rx) = mpsc::channel(4);
        requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id: Some(register::ServiceId::Dtn("mail".to_string())),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(rx))
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::FailedPrecondition);

        harness.bpa.shutdown().await;
    }

    // A dtn-scheme registration binds the dtn endpoint on a node that
    // declares a dtn node id.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dtn_registration_binds_the_dtn_endpoint() {
        let node_ids = NodeIds::try_from(
            [
                NodeId::Ipn(IpnNodeId {
                    allocator_id: 0,
                    node_number: 1,
                }),
                NodeId::Dtn(hardy_bpv7::eid::DtnNodeId {
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

    // An application behind the client SDK: deliveries are pulled to
    // completion through the announced stream and recorded.
    #[cfg(feature = "client")]
    struct SdkApp {
        sink: Once<Box<dyn services::ApplicationSink>>,
        delivered: mpsc::Sender<(Eid, Bytes)>,
        statuses: mpsc::Sender<(bundle::Id, services::StatusNotify)>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Application for SdkApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            bundle_id: &bundle::Id,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _adu_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let payload = hardy_bpa::stream::concat_stream(stream, usize::MAX, None).await?;
            let _ = self
                .delivered
                .send((bundle_id.source.clone(), payload))
                .await;
            Ok(())
        }

        async fn on_status_notify(
            &self,
            bundle_id: &bundle::Id,
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
        let client = crate::client::BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

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

    // An application that receives each delivery whole, then declines it.
    #[cfg(feature = "client")]
    struct DecliningApp {
        sink: Once<Box<dyn services::ApplicationSink>>,
        // Carries the fully received payload of each declined delivery.
        declined: mpsc::Sender<Bytes>,
        unregistered: mpsc::Sender<()>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Application for DecliningApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {
            let _ = self.unregistered.send(()).await;
        }

        async fn on_deliver(
            &self,
            _bundle_id: &bundle::Id,
            _expiry: OffsetDateTime,
            _ack_requested: bool,
            _adu_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let payload = hardy_bpa::stream::concat_stream(stream, usize::MAX, None).await?;
            let _ = self.declined.send(payload).await;
            Err(services::Error::Internal("declined".into()))
        }

        async fn on_status_notify(
            &self,
            _bundle_id: &bundle::Id,
            _from: &Eid,
            _kind: services::StatusNotify,
            _reason: status_report::ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    // The contract across the wire: an application that buffers the
    // whole ADU and then returns `Err` from its `on_deliver` never
    // commits the delivery. The SDK sends no ack, the bundle stays
    // parked, and the endpoint's next registration receives it.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_sdk_decline_after_full_receipt_is_redelivered() {
        let harness = harness().await;
        let client = crate::client::BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

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

        // The decliner received the whole ADU before refusing it.
        let payload = timeout(declined_rx.recv()).await.unwrap();
        assert_eq!(payload, adu);

        // End the declining registration. Its `on_unregister` fires only
        // after the server has freed the identity, so the re-registration
        // below cannot race it.
        sink.unregister().await;
        timeout(unregistered_rx.recv()).await.unwrap();

        // The accepting registration is announced the parked bundle and
        // collects (and commits) it whole.
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

    // A requested delivery report round-trips: the collected delivery
    // generates the report, the BPA consumes it at its admin endpoint,
    // and the sending application is notified through the wire.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_delivery_report_reaches_the_sending_application() {
        let harness = harness().await;
        let client = crate::client::BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

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
                Some(services::SendOptions {
                    notify_delivery: true,
                    ..Default::default()
                }),
                None,
                &mut Bytes::from_static(b"report me"),
            )
            .await
            .unwrap();

        // The SdkApp pulls the delivery to completion, which is what
        // generates the delivered report.
        let _ = timeout(delivered_rx.recv()).await.unwrap();

        let (reported, kind) = timeout(statuses_rx.recv()).await.unwrap();
        assert_eq!(reported, sent);
        assert_eq!(kind, services::StatusNotify::Delivered);

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_registration_re_announces_many_parked_deliveries() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Park a pile of deliveries: announced to this session but
        // never collected. Interleaved send/event pairs keep the
        // session's event buffer shallow.
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

        app.requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(timeout(app.events.message()).await.unwrap().is_none());

        // Re-registration must complete promptly (the poll of parked
        // bundles runs off the registration path) and every parked
        // bundle must be announced again to the new session.
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let mut announced = std::collections::HashSet::new();
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

        // And they are collectable: the pipeline is live end to end.
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

        let mut torn = harness.sessions.torn_down();
        app.requests
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Unregister(Unregister {})),
            })
            .await
            .unwrap();
        assert!(
            timeout(app.events.message()).await.unwrap().is_none(),
            "unregister must end the session stream"
        );

        // The token dies in the session task's teardown, which runs
        // after the stream closes; wait for the teardown signal, so the
        // rejection below is asserted without a race.
        wait_torn_down(&mut torn, &app.token).await;

        let status = send(&mut harness.client, app.token.clone(), "ipn:1.7", b"stale")
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }
}
