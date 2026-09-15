// The application surface: the `hardy.application.v1` wire served
// against the registration surface of a BPA.

use core::{ops::ControlFlow, pin::pin, time::Duration};
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::{
    async_trait,
    bpa::BpaRegistration,
    services::{self, SendOptions},
    stream::{Receiver, Segment},
};
use hardy_bpv7::{
    bundle::Id as BundleId,
    eid::{Eid, Service},
    status_report,
};
use time::OffsetDateTime;
use tokio::{
    sync::mpsc,
    time::{Instant, timeout_at},
};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
#[cfg(feature = "instrument")]
use tracing::instrument;
use tracing::warn;

use super::{service_status, session_sub, verdict};
use crate::{
    MAX_TRANSFER_SIZE,
    application::{
        BundleStatusReport, Delivery, ReceiveMetadata, ReceiveRequest, ReceiveResponse,
        Registration, SendMetadata, SendRequest, SendResponse, StatusAssertion, SubscribeRequest,
        SubscribeResponse, application_service_server::ApplicationService, receive_request,
        register, send_request, subscribe_request, subscribe_response,
    },
    grammar::Cancel,
    server::{
        DATA_CHANNEL_DEPTH, SessionError,
        adapter::{Interrupted, RequestReader, ResponseWriter},
        announce::{Announcements, Collection},
        session::{Session, SessionStream},
        slot::Slot,
        subscribe::{Sessions, SubscribeHandler},
    },
    timestamp::to_timestamp,
    token::Token,
    transfer::Writer,
};

// MAX_TRANSFER_SIZE, tightened to the host's addressable range so an
// oversized transfer ends as a status, not an allocation panic on
// 32-bit targets.
const MAX_ADU_SIZE: u64 = if MAX_TRANSFER_SIZE > isize::MAX as u64 {
    isize::MAX as u64
} else {
    MAX_TRANSFER_SIZE
};

// The component as the BPA sees it.
struct GrpcApplication {
    session: Session<SubscribeResponse>,
    sink: Slot<Arc<dyn services::ApplicationSink>>,
    // Announced deliveries awaiting their Receive call.
    deliveries: Announcements<ReceiveResponse, ReceiveRequest>,
}

impl GrpcApplication {
    fn new(session: Session<SubscribeResponse>) -> Self {
        Self {
            session,
            sink: Slot::new(),
            deliveries: Announcements::default(),
        }
    }

    // One event down the session stream. A torn-down session drops it,
    // which to the BPA is this component's disconnection.
    async fn event(&self, event: subscribe_response::Event) -> services::Result<()> {
        self.session
            .event(SubscribeResponse { event: Some(event) })
            .await
            .map_err(|_| services::Error::Disconnected)
    }
}

impl SubscribeHandler for GrpcApplication {
    type Event = SubscribeResponse;
    type Request = SubscribeRequest;
    // The service id the client asked for, `None` meaning a dynamic
    // registration. `into_register`'s own `Option` answers the different
    // question of whether this was a Register at all.
    type Register = Option<Service>;

    const LABEL: &'static str = "application";

    fn session(&self) -> &Session<SubscribeResponse> {
        &self.session
    }

    fn into_register(request: SubscribeRequest) -> Option<Option<Service>> {
        let Some(subscribe_request::Request::Register(register)) = request.request else {
            return None;
        };
        Some(register.service_id.map(|id| match id {
            register::ServiceId::Ipn(n) => Service::Ipn(n),
            register::ServiceId::Dtn(demux) => Service::Dtn(demux.into()),
        }))
    }

    async fn register(
        bpa: &Arc<dyn BpaRegistration>,
        service: Option<Service>,
        cancel: CancellationToken,
        events_tx: mpsc::Sender<Result<SubscribeResponse, Status>>,
    ) -> Result<(Arc<Self>, SubscribeResponse), Status> {
        // Minted before the BPA sees the component: the session must be
        // able to carry events the moment registration completes.
        let token = Token::mint(&session_sub(Self::LABEL, service.as_ref()));
        let application = Arc::new(GrpcApplication::new(Session::new(
            token.clone(),
            cancel,
            events_tx,
        )));

        let endpoint_id = match service {
            Some(service) => bpa.register_application(service, application.clone()).await,
            None => bpa.register_dynamic_application(application.clone()).await,
        }
        .map_err(service_status)?;

        Ok((
            application,
            SubscribeResponse {
                event: Some(subscribe_response::Event::Registration(Registration {
                    endpoint_id: endpoint_id.to_string(),
                    session_token: token.into(),
                })),
            },
        ))
    }

    async fn on_request(&self, request: SubscribeRequest) -> ControlFlow<()> {
        match request.request {
            // The one message that ends a subscription client-side.
            Some(subscribe_request::Request::Unregister(_)) => ControlFlow::Break(()),
            // Not Debug-formatted: the message carries the session
            // token, which must never reach the logs.
            Some(subscribe_request::Request::Register(_)) | None => {
                warn!("Ignoring unexpected message on the session stream");
                ControlFlow::Continue(())
            }
        }
    }

    async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl services::Application for GrpcApplication {
    async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
        self.sink.set(Arc::from(sink));
    }

    async fn on_unregister(&self) {
        // The session task catches this and runs the one exit sequence.
        self.session.abort();
    }

    async fn on_deliver(
        &self,
        bundle_id: &BundleId,
        expiry: OffsetDateTime,
        ack_requested: bool,
        adu_size: u64,
        stream: &mut dyn Receiver<Segment>,
    ) -> services::Result<()> {
        // Recorded before the Delivery event carries the id to the
        // client, so the Receive door can always find it. Held for the
        // whole exchange: it withdraws on any ending short of the door's
        // collect, a dropped future included.
        let mut announced = self.deliveries.announce(bundle_id);

        self.event(subscribe_response::Event::Delivery(Delivery {
            bundle_id: bundle_id.to_key(),
            source: bundle_id.source.to_string(),
            expire_time: Some(to_timestamp(expiry)),
            ack_requested,
            adu_size,
        }))
        .await?;

        // A collection still running past the expiry is collecting a
        // bundle the BPA has dropped.
        let deadline = Instant::now()
            + (expiry - OffsetDateTime::now_utc())
                .try_into()
                .unwrap_or(Duration::ZERO);
        let cancel = self.session.cancellation();

        // Every ending here leaves the bundle parked for a later
        // collection; only session death is this component's
        // disconnection.
        let Collection {
            responses_tx,
            requests,
        } = timeout_at(deadline, announced.collected(&cancel))
            .await
            .map_err(|_| services::Error::StreamCancelled)?
            .map_err(|e| match e {
                SessionError::Closed => services::Error::Disconnected,
                SessionError::Superseded => services::Error::StreamCancelled,
            })?;

        // Driven inline so the borrowed BPA stream stays alive for it.
        // The commit point is the client's in-band `Ack`, not the
        // in-process handoff.
        let collection = timeout_at(deadline, async {
            let writer = ResponseWriter::new(&responses_tx, &cancel, stream);
            let mut verdict = pin!(verdict(requests));
            let collected = tokio::select! {
                biased;
                // Nothing the client says before the last chunk can
                // commit: an ack here is a protocol violation, since
                // honouring it would finalize a bundle the client
                // provably does not hold. Losing this race drops the
                // writer, so an abandonment reaches the producer at once.
                early = &mut verdict => Err(match early {
                    Ok(()) => Status::invalid_argument("Ack before the final chunk"),
                    Err(status) => status,
                }),
                pushed = writer.write_all() => match pushed {
                    // The response stays open so the client's `Ack` is
                    // observable; it closes with `responses_tx`, as this
                    // call returns.
                    ControlFlow::Continue(()) => tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(services::Error::Disconnected),
                        acked = verdict => acked,
                    },
                    // Both endings have ended the response themselves.
                    ControlFlow::Break(Interrupted::Session) => {
                        return Err(services::Error::Disconnected);
                    }
                    ControlFlow::Break(Interrupted::Broken) => {
                        return Err(services::Error::StreamCancelled);
                    }
                },
            };

            match collected {
                Ok(()) => Ok(()),
                // Awaited, not `try_send`: with the buffer full a
                // dropped status reaches the client as a clean
                // end-of-stream, which it reports as truncation instead
                // of the real cause. The cancel bounds the wait, so a
                // client that stopped draining cannot wedge this call.
                Err(status) => {
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {}
                        _ = responses_tx.send(Err(status)) => {}
                    }
                    Err(services::Error::StreamCancelled)
                }
            }
        });

        match collection.await {
            Ok(collected) => collected,
            // Expired mid-collection: ended like a withdrawn stream.
            Err(_) => {
                let _ = responses_tx.try_send(Ok(ReceiveResponse::cancel()));
                Err(services::Error::StreamCancelled)
            }
        }
    }

    async fn on_status_notify(
        &self,
        bundle_id: &BundleId,
        from: &Eid,
        kind: services::StatusNotify,
        reason: status_report::ReasonCode,
        timestamp: Option<OffsetDateTime>,
    ) {
        // Fire-and-forget: a torn-down session just drops the report.
        let _ = self
            .event(subscribe_response::Event::BundleStatusReport(
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

/// The application surface. Shutting down the pool given to
/// [`new`](Self::new) tears the subscriptions and drives
/// unregistration, so shut it down only after the transport has stopped
/// accepting.
#[derive(Clone)]
pub struct ApplicationServiceImpl {
    sessions: Sessions<GrpcApplication>,
}

impl ApplicationServiceImpl {
    /// Serves the registration surface of `bpa`.
    pub fn new(bpa: Arc<dyn BpaRegistration>, tasks: TaskPool) -> Self {
        Self {
            sessions: Sessions::new(bpa, tasks),
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
        self.sessions.subscribe(request.into_inner()).await
    }

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
        let application = self.sessions.resolve(session_token)?;
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

        // Rejected up front rather than streamed to the ceiling and
        // then failed.
        if adu_size.is_some_and(|size| size > MAX_ADU_SIZE) {
            return Err(Status::resource_exhausted(
                "Declared ADU size exceeds the maximum transfer size",
            ));
        }

        let options = options.map(SendOptions::from);

        // The BPA pulls chunk by chunk and assembles the ADU behind its
        // own size bound, so nothing materialises in the server.
        let mut reader = RequestReader::new(requests, cancelled, "Send");
        match application
            .sink
            .get()
            .ok_or_else(|| service_status(services::Error::Disconnected))?
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
        let application = self.sessions.resolve(session_token)?;
        let bundle_id =
            BundleId::from_key(&bundle_id).map_err(|_| Status::not_found("No such delivery"))?;

        // A single-use take, so this call is the delivery's sole
        // collector; never announced, already collected, and withdrawn
        // all answer not-found.
        let Some(call_tx) = application.deliveries.collect(&bundle_id) else {
            return Err(Status::not_found("No such delivery"));
        };

        // `on_deliver` drives the transfer through these streams, so the
        // bundle bytes never materialise in the server.
        let (responses_tx, responses_rx) = mpsc::channel(DATA_CHANNEL_DEPTH);
        if call_tx
            .send(Collection {
                responses_tx,
                requests,
            })
            .is_err()
        {
            // `on_deliver` stopped awaiting between the announce and
            // now, so the delivery is no longer live.
            return Err(Status::not_found("No such delivery"));
        }
        Ok(Response::new(ReceiverStream::new(responses_rx)))
    }
}

// The wire against a real BPA.
#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, time::Duration};

    #[cfg(feature = "client")]
    use crate::client::BpaClient;
    #[cfg(feature = "client")]
    use hardy_async::sync::spin::Once;
    #[cfg(feature = "client")]
    use hardy_bpa::stream::concat_stream;
    use hardy_bpa::{
        Bytes,
        bpa::Bpa,
        cla::{self, Cla, ClaInit},
        node_ids::NodeIds,
        policy::FlowControllerFactory,
        routing::{self, RoutingAgent},
        // `Service` is the eid's here, so the BPA's low-level service
        // trait is the one aliased.
        services::Service as BpaService,
    };
    use hardy_bpv7::eid::{DtnNodeId, IpnNodeId, NodeId};
    use tokio::sync::Notify;
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
    #[cfg(feature = "client")]
    use crate::client::MAX_CONCURRENT_DELIVERIES;
    use crate::server::{CHANNEL_DEPTH, DATA_CHANNEL_DEPTH, subscribe::Sessions};

    struct Harness {
        bpa: Arc<Bpa>,
        // Held live: dropping the pool would tear the sessions.
        tasks: TaskPool,
        client: ApplicationServiceClient<Channel>,
        #[cfg_attr(
            not(feature = "client"),
            expect(dead_code, reason = "read by the client SDK test")
        )]
        address: SocketAddr,
        // The session index, for the teardown barrier.
        sessions: Sessions<GrpcApplication>,
    }

    // A running BPA (node ipn:1) behind the surface on a port-0
    // listener, plus a connected generated client.
    async fn harness() -> Harness {
        harness_with(ipn1()).await
    }

    async fn harness_with(node_ids: NodeIds) -> Harness {
        // On for the report round-trip test.
        let bpa = build_bpa(node_ids, true).await;

        let tasks = TaskPool::new();
        let service_impl = ApplicationServiceImpl::new(bpa.clone(), tasks.clone());
        let sessions = service_impl.sessions.clone();
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
        requests_tx: mpsc::Sender<SubscribeRequest>,
        events: Streaming<SubscribeResponse>,
        endpoint_id: String,
        token: Bytes,
    }

    // Opens a session and completes the registration handshake.
    async fn register(
        client: &mut ApplicationServiceClient<Channel>,
        service_id: Option<register::ServiceId>,
    ) -> App {
        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id,
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
            panic!("expected the Registration event first, got {event:?}");
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
    // ack; `abandon` cancels instead and surfaces the terminal status.
    async fn collect(
        client: &mut ApplicationServiceClient<Channel>,
        token: Bytes,
        bundle_id: &str,
        abandon: bool,
    ) -> Result<Vec<u8>, Status> {
        // Kept open for the whole collection, as the SDK does: metadata
        // first, then Ack to commit or Cancel to abandon.
        let (requests_tx, requests_rx) = tokio::sync::mpsc::channel(4);
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
            .receive(tokio_stream::wrappers::ReceiverStream::new(requests_rx))
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
                        // The final chunk may already be queued, and
                        // only the terminal status ends an abandonment.
                        continue;
                    }
                    let _ = requests_tx
                        .send(ReceiveRequest {
                            request: Some(receive_request::Request::Ack(())),
                        })
                        .await;
                    // Drained to EOS so the ack reaches the server
                    // before the call is dropped.
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

    // Sends `adu` as a chunked transfer, for the tests that need
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

    // Takes a Receive through its last chunk without acknowledging,
    // leaving the request side open for the caller's verdict.
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
                other => panic!("expected a chunk, got {other:?}"),
            }
        }
        (requests_tx, stream, collected)
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

    // The deferred-not-lost tail every parking test shares: end the
    // session, then collect the bundle whole on the next registration.
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

        // The client vanishes without Unregister. Subscribing to the
        // teardown signal before dropping asserts the rejection below
        // without a race; the timeout only bounds a regression.
        let mut torn = harness.sessions.torn_down();
        drop(app.events);
        drop(app.requests_tx);
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

        // The pool's token is the parent of every session trigger, so
        // shutdown must end the stream with no client action.
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

    // Above the bound is rejected before any bytes arrive; within it
    // the declaration is a hint, and an inaccurate one still commits.
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

    // The announcement is recorded before the Delivery event goes out,
    // so an early NOT_FOUND neither consumes nor poisons anything.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_receive_racing_the_announcement_lands() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        let adu = b"raced";
        let destination = app.endpoint_id.clone();
        let sent = send(&mut harness.client, app.token.clone(), &destination, adu)
            .await
            .unwrap();

        // Polled without reading the session stream: early attempts may
        // answer NOT_FOUND, and the first success collects the whole ADU.
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

    // A session dying with a claimed Receive mid-stream leaves the
    // bundle parked for the next registration.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_death_mid_receive_defers_the_delivery() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Far more than the server's buffer plus transport windows can
        // absorb, so a client that does not read parks the writer well
        // before the final segment.
        let adu = vec![0x5a; 16 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Awaiting the call means the handler ran, so the stream is
        // claimed and its first pull probed. Then read nothing.
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

        // Kill the session with the collection mid-stream.
        app.requests_tx
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

    // Completion is the ack, not the last chunk: a cancel after the
    // final chunk still abandons, and the bundle is re-announced.
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

        // The last chunk is in hand but no ack was sent: the cancel is
        // the verdict, and the call ends with the abandonment status.
        requests_tx
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

    // A client that takes the whole ADU and then goes silent commits
    // nothing, and the bundle is re-announced.
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

        // A request stream that ends without an ack is an abandonment,
        // not the inert half-close it is on the session stream.
        drop(requests_tx);
        assert_eq!(terminal_status(&mut stream).await.code(), Code::Cancelled);

        // Parked, not finalized.
        recollected_after_reregistration(&mut harness, app, adu).await;

        harness.bpa.shutdown().await;
    }

    // An ack racing the drain is a protocol violation, never a commit:
    // the collection ends without its last chunk and the bundle is
    // re-announced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ack_before_the_final_chunk_never_commits() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // More than the buffer plus transport windows can absorb, so
        // the drain cannot complete before the early ack is seen.
        // Derived from the constant so a larger buffer cannot make the
        // test vacuous.
        let adu = vec![0x5a; DATA_CHANNEL_DEPTH * 4 * crate::CHUNK_SIZE];
        let destination = app.endpoint_id.clone();
        send_chunked(&mut harness.client, app.token.clone(), destination, &adu).await;
        let announced = delivery(&mut app, adu.len() as u64).await;

        // Ack straight after the metadata, before reading a single chunk.
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

        // Truncation, never completion. The INVALID_ARGUMENT status is
        // best-effort: it fires against a full response channel, so the
        // refusal may surface as a bare truncation instead.
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

    // A client that claims a large collection and stops reading must
    // not wedge pool shutdown: the parked writer abandons its terminal
    // status rather than await a channel nothing drains.
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
        let _claimed = harness
            .client
            .receive(ReceiverStream::new(requests_rx))
            .await
            .unwrap()
            .into_inner();

        // Shutdown must drain despite the parked, unread writer.
        timeout(harness.tasks.shutdown()).await;

        harness.bpa.shutdown().await;
    }

    // A stalled session must not starve the pipeline: other
    // registrations keep delivering.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stalled_session_does_not_starve_other_registrations() {
        let mut harness = harness().await;
        // Registered, then never read again: its event buffer fills.
        let stalled = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;
        let mut healthy = register(&mut harness.client, Some(register::ServiceId::Ipn(8))).await;

        // Past the session event buffer, derived from the constant so
        // the flood cannot shrink to fit it and go vacuous.
        let flood = CHANNEL_DEPTH + CHANNEL_DEPTH / 2;
        for i in 0..flood {
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

        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id: Some(register::ServiceId::Dtn("mail".to_string())),
                })),
            })
            .await
            .unwrap();
        let status = harness
            .client
            .subscribe(ReceiverStream::new(requests_rx))
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

    // An application behind the client SDK: deliveries are pulled to
    // completion through the announced stream and recorded.
    #[cfg(feature = "client")]
    struct SdkApp {
        sink: Once<Box<dyn services::ApplicationSink>>,
        delivered: mpsc::Sender<(Eid, Bytes)>,
        statuses: mpsc::Sender<(BundleId, services::StatusNotify)>,
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
        let client = BpaClient::new(
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

    // An application that echoes each delivery back out through its own
    // sink from inside `on_deliver`: the echo-over-gRPC shape.
    #[cfg(feature = "client")]
    struct EchoApp {
        sink: Once<Box<dyn services::ApplicationSink>>,
        // Where each received delivery is echoed to.
        peer: Eid,
    }

    #[cfg(feature = "client")]
    #[async_trait]
    impl services::Application for EchoApp {
        async fn on_register(&self, _source: &Eid, sink: Box<dyn services::ApplicationSink>) {
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
            // The reply is issued from inside the delivery, on the delivery
            // task: the SDK must not serialise the two against each other.
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
            _reason: status_report::ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    // Replying from within a pulled delivery must not deadlock, even
    // with more echoes in flight than the SDK's concurrency bound.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_sdk_reply_from_within_a_delivery_does_not_deadlock() {
        let harness = harness().await;
        let client = BpaClient::new(
            format!("http://{}", harness.address),
            hardy_async::TaskPool::new(),
        )
        .unwrap();

        // The collector both seeds the echo app and receives every echo.
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

        // Far more than the SDK's concurrent-delivery bound, so replies are
        // generated from several deliveries in flight at once.
        let count = MAX_CONCURRENT_DELIVERIES.get() * 8;
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

        // A reply that deadlocked inside a delivery would leave this
        // short; the timeout only bounds a regression.
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
            _reason: status_report::ReasonCode,
            _timestamp: Option<OffsetDateTime>,
        ) {
        }
    }

    // An application that buffers the whole ADU and then returns `Err`
    // never commits: no ack goes out, and the bundle stays parked.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_sdk_decline_after_full_receipt_is_redelivered() {
        let harness = harness().await;
        let client = BpaClient::new(
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

        // `on_unregister` fires only after the server has freed the
        // identity, so the re-registration below cannot race it.
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

    // A requested delivery report round-trips to the sending
    // application through the wire.
    #[cfg(feature = "client")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_delivery_report_reaches_the_sending_application() {
        let harness = harness().await;
        let client = BpaClient::new(
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

    #[ignore = "the BPA serialises deliveries per service, so a held-open collection blocks the next announcement: see docs/TODO.md"]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn re_registration_re_announces_many_parked_deliveries() {
        let mut harness = harness().await;
        let mut app = register(&mut harness.client, Some(register::ServiceId::Ipn(7))).await;

        // Announced to this session but never collected. Interleaved
        // send/event pairs keep the event buffer shallow.
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

        // Re-registration must complete promptly, and every parked
        // bundle be announced again to the new session.
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

        // Teardown runs after the stream closes, so the signal is what
        // makes the rejection below race-free.
        wait_torn_down(&mut torn, &app.token).await;

        let status = send(&mut harness.client, app.token.clone(), "ipn:1.7", b"stale")
            .await
            .unwrap_err();
        assert_eq!(status.code(), Code::Unauthenticated);

        harness.bpa.shutdown().await;
    }

    // Hands the surface a sink, then parks inside
    // `register_application` until released: the shape every registry
    // has while it awaits after committing. Only that method is
    // reachable.
    struct ParkedBpa {
        registered: mpsc::Sender<()>,
        release: Arc<Notify>,
        unregistered: mpsc::Sender<()>,
    }

    struct ParkedSink {
        unregistered: mpsc::Sender<()>,
    }

    #[async_trait]
    impl services::ApplicationSink for ParkedSink {
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
            // The registration is committed; the rpc can be abandoned
            // from here, and this call still completes.
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
            unreachable!("the application surface registers applications only")
        }

        async fn register_service(
            &self,
            _service_id: Service,
            _service: Arc<dyn BpaService>,
        ) -> services::Result<Eid> {
            unreachable!("the application surface registers applications only")
        }

        async fn register_dynamic_service(
            &self,
            _service: Arc<dyn BpaService>,
        ) -> services::Result<Eid> {
            unreachable!("the application surface registers applications only")
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
            unreachable!("the application surface registers applications only")
        }
    }

    // Fires when the rpc handler future is dropped rather than
    // completed: the only thing tonic exposes that says the server
    // processed the client's reset. The window this test pins closes
    // when registration returns, so without it the test goes vacuous.
    struct HandlerDropped(Option<mpsc::Sender<()>>);

    impl Drop for HandlerDropped {
        fn drop(&mut self) {
            if let Some(dropped_tx) = self.0.take() {
                let _ = dropped_tx.try_send(());
            }
        }
    }

    // The surface, unchanged, with that signal held across `subscribe`.
    #[derive(Clone)]
    struct WatchedApplication {
        inner: ApplicationServiceImpl,
        dropped_tx: mpsc::Sender<()>,
    }

    #[async_trait]
    impl ApplicationService for WatchedApplication {
        type SubscribeStream = SessionStream<SubscribeResponse>;
        type ReceiveStream = ReceiverStream<Result<ReceiveResponse, Status>>;

        async fn subscribe(
            &self,
            request: Request<Streaming<SubscribeRequest>>,
        ) -> Result<Response<Self::SubscribeStream>, Status> {
            let mut dropped = HandlerDropped(Some(self.dropped_tx.clone()));
            let response = self.inner.subscribe(request).await;
            // Returned, so not abandoned.
            dropped.0 = None;
            response
        }

        async fn send(
            &self,
            request: Request<Streaming<SendRequest>>,
        ) -> Result<Response<SendResponse>, Status> {
            self.inner.send(request).await
        }

        async fn receive(
            &self,
            request: Request<Streaming<ReceiveRequest>>,
        ) -> Result<Response<Self::ReceiveStream>, Status> {
            self.inner.receive(request).await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_rpc_abandoned_during_registration_ends_unregistered() {
        let (registered_tx, mut registered_rx) = mpsc::channel(1);
        let (unregistered_tx, mut unregistered_rx) = mpsc::channel(1);
        let release = Arc::new(Notify::new());

        let (dropped_tx, mut dropped_rx) = mpsc::channel(1);

        let tasks = TaskPool::new();
        let service = ApplicationServiceServer::new(WatchedApplication {
            inner: ApplicationServiceImpl::new(
                Arc::new(ParkedBpa {
                    registered: registered_tx,
                    release: release.clone(),
                    unregistered: unregistered_tx,
                }),
                tasks.clone(),
            ),
            dropped_tx,
        });
        let address = serve(Server::builder().add_service(service)).await;
        let mut client = ApplicationServiceClient::connect(format!("http://{address}"))
            .await
            .unwrap();

        let (requests_tx, requests_rx) = mpsc::channel(4);
        requests_tx
            .send(SubscribeRequest {
                request: Some(subscribe_request::Request::Register(Register {
                    service_id: Some(register::ServiceId::Ipn(7)),
                })),
            })
            .await
            .unwrap();
        let call =
            tokio::spawn(async move { client.subscribe(ReceiverStream::new(requests_rx)).await });

        // The registration has committed and the register call is
        // parked, exactly where nothing else watches the subscription.
        timeout(registered_rx.recv()).await.unwrap();

        // Aborting resets the http/2 stream, so the handler waiting for
        // the subscription is dropped.
        call.abort();
        drop(requests_tx);

        // The reset must reach the server before registration returns,
        // or the window is never entered.
        timeout(dropped_rx.recv()).await.unwrap();

        // Now let the registration complete, into an rpc that is no
        // longer there to be given the subscription.
        release.notify_one();

        // The regression: an rpc that did not survive its own
        // registration still unregisters it.
        timeout(unregistered_rx.recv()).await.unwrap();

        tasks.shutdown().await;
    }
}
