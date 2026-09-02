// The service surface: the `service.v1` wire served against the
// low-level service surface of a BPA. Declarations are ordered
// define-before-reference: the wire conversions, the component as the
// BPA sees it, the doors' streaming halves, then the rpc service;
// within the service, the handlers mirror the schema's order. Unlike
// the application surface, Send streams straight into the BPA through
// `ServiceSink::send`: bundle bytes are never materialised in the
// bridge.

use std::sync::Arc;

use hardy_async::TaskPool;
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    services,
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
    server::{
        CHANNEL_DEPTH, adapter,
        session::{Session, SessionStream},
    },
    service::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse,
        Registration, SendMetadata, SendRequest, SendResponse, StatusAssertion, SubscribeRequest,
        SubscribeResponse, receive_request, register, send_request,
        service_service_server::ServiceService, subscribe_request, subscribe_response,
    },
};

// -------------------------------------------------------------------
// The component as the BPA sees it
// -------------------------------------------------------------------

struct GrpcService {
    session: Session<SubscribeResponse>,
    sink: SinkSlot<dyn services::ServiceSink>,
    // Announced streams held for the Receive door: the single
    // collection capability per announcement.
    deliveries: Deliveries,
}

impl GrpcService {
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

impl Component for GrpcService {
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
impl services::Service for GrpcService {
    async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
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
        bundle_size: u64,
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
                        expire_time: Some(to_timestamp(expiry)),
                        bundle_size,
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

/// The service bridge. Shutting down the pool given to
/// [`new`](Self::new) tears the sessions and drives unregistration,
/// so shut it down only after the transport has stopped accepting.
#[derive(Clone)]
pub struct ServiceServiceImpl {
    bridge: Bridge<GrpcService>,
}

impl ServiceServiceImpl {
    /// Bridges the low-level service surface of `bpa`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            bridge: Bridge::new(bpa, tasks),
        }
    }
}

#[async_trait]
impl ServiceService for ServiceServiceImpl {
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
        let service_id = register.service_id.map(|id| match id {
            register::ServiceId::Ipn(n) => Service::Ipn(n),
            register::ServiceId::Dtn(demux) => Service::Dtn(demux.into()),
        });

        // The token is minted before the BPA sees the component: the
        // session must be able to carry events the moment registration
        // completes. The token's `sub` prefix is the requested identity,
        // observability only; the reply carries the resolved endpoint.
        let sub = match &service_id {
            Some(Service::Ipn(n)) => format!("ipn:{n}"),
            Some(Service::Dtn(name)) => format!("dtn:{name}"),
            None => "dynamic".to_string(),
        };
        let token = self.bridge.sessions.mint(&sub);
        let (events_tx, events_rx) = mpsc::channel(CHANNEL_DEPTH);
        let service = Arc::new(GrpcService::new(Session::new(
            token.clone(),
            self.bridge.tasks.child_token(),
            events_tx,
        )));

        let endpoint_id = match service_id {
            Some(service_id) => {
                self.bridge
                    .bpa
                    .register_service(service_id, service.clone())
                    .await
            }
            None => {
                self.bridge
                    .bpa
                    .register_dynamic_service(service.clone())
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
            service,
            registration,
            events_rx,
            requests,
            "service_session",
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

        let Some(send_request::Request::Metadata(SendMetadata { session_token })) =
            requests.message().await?.and_then(|r| r.request)
        else {
            return Err(Status::invalid_argument(
                "The first message must be the metadata",
            ));
        };
        let service = self.bridge.sessions.resolve(session_token)?;

        // The BPA pulls the transfer chunk by chunk, parses and
        // validates the assembled bundle (services are not trusted),
        // and caps the reassembly with its own bundle size limit.
        let mut reader = adapter::Reader::new(requests, service.session.cancellation(), "Send");
        match service.sink.get()?.send(&mut reader).await {
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
        let service = self.bridge.sessions.resolve(session_token)?;
        let response = service
            .deliveries
            .collect(
                &self.bridge.tasks,
                service.session.cancellation(),
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
    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    use hardy_bpa::{Bytes, bpa::Bpa};
    use tonic::{
        Code,
        transport::{Channel, Server},
    };

    use super::{
        super::tests::{build_bpa, build_bundle, ipn1, serve, timeout, wait_torn_down},
        *,
    };
    use crate::server::session::Sessions;
    use crate::service::{
        Register, Unregister, receive_response, service_service_client::ServiceServiceClient,
        service_service_server::ServiceServiceServer,
    };

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear the sessions.
        #[expect(dead_code, reason = "held for its liveness")]
        tasks: TaskPool,
        client: ServiceServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: std::net::SocketAddr,
        // The session index, for the teardown barrier.
        sessions: Arc<Sessions<GrpcService>>,
    }

    // A running BPA (node ipn:1) behind the bridge on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        // Status reports on: the report round-trip test needs them.
        let bpa = build_bpa(ipn1(), true).await;

        let tasks = TaskPool::new();
        let service_impl = ServiceServiceImpl::new(bpa.clone(), tasks.clone());
        let sessions = service_impl.bridge.sessions.clone();
        let service = ServiceServiceServer::new(service_impl);
        let address = serve(Server::builder().add_service(service)).await;

        let client = ServiceServiceClient::connect(format!("http://{address}"))
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

    struct Registered {
        requests: mpsc::Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        endpoint_id: String,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(
        client: &mut ServiceServiceClient<Channel>,
        service_id: Option<register::ServiceId>,
    ) -> Registered {
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

        Registered {
            requests,
            events,
            endpoint_id: registration.endpoint_id,
            token: registration.session_token,
        }
    }

    // A canonical BPv7 bundle as raw bytes, as a registered service
    // would build one.
    async fn send(
        client: &mut ServiceServiceClient<Channel>,
        token: Bytes,
        bundle: Bytes,
    ) -> Result<SendResponse, Status> {
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: token,
                })),
            },
            SendRequest {
                request: Some(send_request::Request::LastChunk(bundle)),
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
        client: &mut ServiceServiceClient<Channel>,
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

    // Awaits the Delivery announcing `bundle_size` bytes on the
    // session stream.
    async fn delivery(registered: &mut Registered, bundle_size: u64) -> Delivery {
        loop {
            let event = timeout(registered.events.message()).await.unwrap().unwrap();
            match event.event {
                Some(subscribe_response::Event::Delivery(delivery)) => {
                    assert_eq!(delivery.bundle_size, bundle_size);
                    return delivery;
                }
                // Status reports about the sent bundle may interleave.
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
        assert!(!dynamic.endpoint_id.is_empty());
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

        // Collection returns the bundle exactly as stored: the builder
        // emits canonical bytes, so they round-trip unchanged.
        let collected = collect(
            &mut harness.client,
            registered.token.clone(),
            &delivery.bundle_id,
            false,
        )
        .await
        .unwrap();
        assert_eq!(collected, bundle);

        // The completed collection consumed the delivery: the sent and
        // announced ids were the same real bundle id, and it is gone.
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
    async fn a_truncated_send_never_commits() {
        let mut harness = harness().await;
        let mut registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // A chunk but no last chunk: the half-close is a truncation,
        // not a commit.
        let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, b"cut");
        let messages = [
            SendRequest {
                request: Some(send_request::Request::Metadata(SendMetadata {
                    session_token: registered.token.clone(),
                })),
            },
            SendRequest {
                request: Some(send_request::Request::Chunk(bundle)),
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
        while let Some(event) = registered.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a truncated send must not deliver"
            );
        }
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
                })),
            },
            SendRequest {
                request: Some(send_request::Request::Chunk(bundle)),
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
        while let Some(event) = registered.events.message().await.unwrap() {
            assert!(
                !matches!(event.event, Some(subscribe_response::Event::Delivery(_))),
                "a cancelled send must not deliver"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_invalid_bundle_is_rejected() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // The BPA parses and validates at the security boundary: raw
        // garbage never enters the store.
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

        let payload = vec![0x5a; crate::CHUNK_SIZE + 3];
        let bundle = build_bundle(&registered.endpoint_id, &registered.endpoint_id, &payload);
        send(
            &mut harness.client,
            registered.token.clone(),
            bundle.clone(),
        )
        .await
        .unwrap();
        let first = delivery(&mut registered, bundle.len() as u64).await;

        // Abandoning with an in-band cancel ends the collection without
        // acknowledging it, with the abandonment status; the bundle is
        // parked, not finalized.
        let abandoned = collect(
            &mut harness.client,
            registered.token.clone(),
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
            registered.token.clone(),
            &first.bundle_id,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(spent.code(), Code::NotFound);

        // Deferred, not lost: the next registration is announced the
        // parked bundle afresh and collects the whole bundle.
        registered
            .requests
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

        // The registration's endpoint is the only source it may
        // originate from: a bundle claiming another endpoint is
        // rejected at the security boundary.
        let bundle = build_bundle("ipn:1.99", "ipn:1.7", b"forged source");
        let status = send(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);

        harness.bpa.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_stream_tears_the_session_down() {
        let mut harness = harness().await;
        let registered = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // The client vanishes without Unregister: dropping the rpc's
        // streams is caught by the response-stream guard and the request
        // half-close, and the session tears down. Wait for the teardown
        // signal, so the rejection below is asserted without a race.
        let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"stale");
        let mut torn = harness.sessions.torn_down();
        drop(registered.events);
        drop(registered.requests);
        wait_torn_down(&mut torn, &registered.token).await;

        let status = send(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    // A service behind the client SDK: deliveries are pulled to
    // completion through the announced stream and recorded.
    #[cfg(feature = "client")]
    struct SdkService {
        sink: Once<Box<dyn services::ServiceSink>>,
        delivered: mpsc::Sender<Bytes>,
        statuses: mpsc::Sender<(bundle::Id, services::StatusNotify)>,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Service for SdkService {
        async fn on_register(&self, _endpoint: &Eid, sink: Box<dyn services::ServiceSink>) {
            self.sink.call_once(|| sink);
        }

        async fn on_unregister(&self) {}

        async fn on_deliver(
            &self,
            _bundle_id: &bundle::Id,
            _expiry: OffsetDateTime,
            _bundle_size: u64,
            stream: &mut dyn Receiver<Segment>,
        ) -> services::Result<()> {
            let data = hardy_bpa::stream::concat_stream(stream, usize::MAX, None).await?;
            let _ = self.delivered.send(data).await;
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
        let remote =
            crate::client::BpaClient::new(format!("http://{}", harness.address), TaskPool::new())
                .unwrap();

        let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
        let (statuses_tx, _statuses_rx) = mpsc::channel(4);
        let svc = Arc::new(SdkService {
            sink: Once::new(),
            delivered: delivered_tx,
            statuses: statuses_tx,
        });
        let eid = remote
            .register_service(Service::Ipn(9), svc.clone())
            .await
            .unwrap();
        assert_eq!(eid.to_string(), "ipn:1.9");

        // A whole buffer is one final segment through the pump,
        // sliced into wire chunks.
        let bundle = build_bundle("ipn:1.9", "ipn:1.9", b"through the sdk as a whole bundle");
        let sink = svc.sink.get().unwrap();
        sink.send(&mut bundle.clone()).await.unwrap();

        let data = timeout(delivered_rx.recv()).await.unwrap();
        assert_eq!(data, bundle);

        sink.unregister().await;
        harness.bpa.shutdown().await;
    }

    // A service-built bundle that requests a delivery report gets the
    // report back through the wire: the service sets the flag and the
    // report-to itself, the BPA consumes the report at its admin
    // endpoint, and the origin registration is notified.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_delivery_report_reaches_the_sending_service() {
        let harness = harness().await;
        let remote = crate::client::BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        let (delivered_tx, mut delivered_rx) = mpsc::channel(4);
        let (statuses_tx, mut statuses_rx) = mpsc::channel(4);
        let svc = Arc::new(SdkService {
            sink: Once::new(),
            delivered: delivered_tx,
            statuses: statuses_tx,
        });
        remote
            .register_service(Service::Ipn(9), svc.clone())
            .await
            .unwrap();

        // A raw bundle to self, flagged for a delivery report, with
        // the node's admin endpoint as report-to.
        let (built, data) = hardy_bpv7::builder::Builder::new(
            "ipn:1.9".parse().unwrap(),
            "ipn:1.9".parse().unwrap(),
        )
        .with_flags(hardy_bpv7::bundle::Flags {
            delivery_report_requested: true,
            ..Default::default()
        })
        .with_report_to("ipn:1.0".parse().unwrap())
        .with_payload(std::borrow::Cow::Borrowed(b"report me"))
        .build(hardy_bpv7::creation_timestamp::CreationTimestamp::now())
        .unwrap();

        let sink = svc.sink.get().unwrap();
        let sent = sink.send(&mut Bytes::from(data)).await.unwrap();
        assert_eq!(sent, built.primary.id);

        // The SdkService pulls the delivery to completion, which is
        // what generates the delivered report.
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
        let mut torn = harness.sessions.torn_down();

        registered
            .requests
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

        // The token dies in the session task's teardown, which runs
        // after the stream closes; wait for the teardown signal, so the
        // rejection below is asserted without a race.
        let bundle = build_bundle("ipn:1.7", "ipn:1.7", b"stale");
        wait_torn_down(&mut torn, &registered.token).await;
        let status = send(&mut harness.client, registered.token.clone(), bundle)
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }
}
