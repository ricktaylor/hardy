use crate::contacts::{Contact, Schedule, TvrAgent};
use crate::cron::CronExpr;
use hardy_bpa::routes::Action;
use std::collections::HashSet;
use std::sync::Arc;
use tracing::{debug, info, warn};

mod proto {
    pub mod tvr {
        tonic::include_proto!("tvr");
    }

    pub mod google {
        pub mod rpc {
            tonic::include_proto!("google.rpc");

            impl From<tonic::Status> for Status {
                fn from(value: tonic::Status) -> Self {
                    Self {
                        code: value.code().into(),
                        message: value.message().to_string(),
                        details: Vec::new(),
                    }
                }
            }
        }
    }
}

use proto::tvr::*;

// ── Proto → internal conversion ─────────────────────────────────────

fn convert_timestamp(t: prost_types::Timestamp) -> Result<time::OffsetDateTime, tonic::Status> {
    time::OffsetDateTime::from_unix_timestamp(t.seconds)
        .map(|dt| dt + time::Duration::nanoseconds(t.nanos.into()))
        .map_err(|e| tonic::Status::invalid_argument(format!("invalid timestamp: {e}")))
}

fn convert_duration(d: prost_types::Duration) -> Result<std::time::Duration, tonic::Status> {
    if d.seconds < 0 || d.nanos < 0 {
        return Err(tonic::Status::invalid_argument("duration must be positive"));
    }
    Ok(std::time::Duration::new(d.seconds as u64, d.nanos as u32))
}

fn convert_contact(proto: proto::tvr::Contact) -> Result<Contact, tonic::Status> {
    let pattern = proto
        .pattern
        .parse()
        .map_err(|e| tonic::Status::invalid_argument(format!("invalid EID pattern: {e}")))?;

    let action = match proto.action {
        Some(contact::Action::Via(eid)) => {
            let eid = eid.parse().map_err(|e| {
                tonic::Status::invalid_argument(format!("invalid next-hop EID: {e}"))
            })?;
            Action::Via(eid)
        }
        Some(contact::Action::Drop(drop_action)) => {
            let reason = if drop_action.reason_code == 0 {
                None
            } else {
                Some((drop_action.reason_code as u64).try_into().map_err(|e| {
                    tonic::Status::invalid_argument(format!("invalid reason code: {e}"))
                })?)
            };
            Action::Drop(reason)
        }
        None => {
            return Err(tonic::Status::invalid_argument(
                "contact must have an action",
            ));
        }
    };

    let schedule = match proto.schedule {
        Some(contact::Schedule::OneShot(one_shot)) => {
            let start = one_shot.start.map(convert_timestamp).transpose()?;
            let end = one_shot.end.map(convert_timestamp).transpose()?;
            if let (Some(s), Some(e)) = (start, end)
                && e <= s
            {
                return Err(tonic::Status::invalid_argument(
                    "'end' must be after 'start'",
                ));
            }
            Schedule::OneShot { start, end }
        }
        Some(contact::Schedule::Recurring(recurring)) => {
            let cron = CronExpr::parse(&recurring.cron)
                .map_err(|e| tonic::Status::invalid_argument(format!("invalid cron: {e}")))?;
            let duration = recurring
                .duration
                .map(convert_duration)
                .transpose()?
                .ok_or_else(|| {
                    tonic::Status::invalid_argument("recurring contact requires duration")
                })?;
            if duration.is_zero() {
                return Err(tonic::Status::invalid_argument(
                    "duration must be greater than zero",
                ));
            }
            let until = recurring.until.map(convert_timestamp).transpose()?;
            Schedule::Recurring {
                cron,
                duration,
                until,
            }
        }
        None => Schedule::Permanent,
    };

    Ok(Contact {
        pattern,
        action,
        priority: proto.priority,
        schedule,
        bandwidth_bps: proto.bandwidth_bps,
        delay_us: proto.delay_us,
    })
}

fn convert_contacts(protos: Vec<proto::tvr::Contact>) -> Result<Vec<Contact>, tonic::Status> {
    protos.into_iter().map(convert_contact).collect()
}

// ── Service ─────────────────────────────────────────────────────────

pub struct TvrService {
    agent: Arc<TvrAgent>,
    tasks: hardy_async::TaskPool,
    active_sessions: Arc<hardy_async::sync::spin::Mutex<HashSet<String>>>,
}

impl TvrService {
    pub fn new(agent: &Arc<TvrAgent>, tasks: &hardy_async::TaskPool) -> Self {
        Self {
            agent: agent.clone(),
            tasks: tasks.clone(),
            active_sessions: Arc::new(hardy_async::sync::spin::Mutex::new(HashSet::new())),
        }
    }
}

#[tonic::async_trait]
impl tvr_server::Tvr for TvrService {
    type SessionStream =
        tokio_stream::wrappers::ReceiverStream<Result<ServerMessage, tonic::Status>>;

    async fn session(
        &self,
        request: tonic::Request<tonic::Streaming<ClientMessage>>,
    ) -> Result<tonic::Response<Self::SessionStream>, tonic::Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let stream = request.into_inner();
        let agent = self.agent.clone();
        let cancel = self.tasks.cancel_token().clone();
        let active_sessions = self.active_sessions.clone();

        hardy_async::spawn!(&self.tasks, "tvr_session", async move {
            run_session(stream, tx, agent, cancel, active_sessions).await;
        });

        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ))
    }
}

async fn run_session(
    mut stream: tonic::Streaming<ClientMessage>,
    tx: tokio::sync::mpsc::Sender<Result<ServerMessage, tonic::Status>>,
    agent: Arc<TvrAgent>,
    cancel: hardy_async::CancellationToken,
    active_sessions: Arc<hardy_async::sync::spin::Mutex<HashSet<String>>>,
) {
    // First message must be OpenSession
    let (session_name, default_priority) = match stream.message().await {
        Ok(Some(ClientMessage {
            msg_id,
            msg: Some(client_message::Msg::Open(open)),
        })) => {
            let name = open.name.clone();

            // Reject duplicate session names
            if !active_sessions.lock().insert(name.clone()) {
                warn!("TVR session name already in use: '{name}'");
                let _ = tx
                    .send(Err(tonic::Status::already_exists(format!(
                        "session name '{name}' is already in use"
                    ))))
                    .await;
                return;
            }

            let priority = if open.default_priority == 0 {
                agent.default_priority()
            } else {
                open.default_priority
            };

            info!("TVR session opened: '{name}' (priority {priority})");
            metrics::gauge!("tvr_sessions").increment(1.0);

            let response = ServerMessage {
                msg_id,
                msg: Some(server_message::Msg::Open(OpenSessionResponse {})),
            };
            if tx.send(Ok(response)).await.is_err() {
                active_sessions.lock().remove(&name);
                return;
            }

            (name, priority)
        }
        Ok(Some(_)) => {
            warn!("First message must be OpenSession");
            let _ = tx
                .send(Err(tonic::Status::invalid_argument(
                    "First message must be OpenSessionRequest",
                )))
                .await;
            return;
        }
        Ok(None) => {
            debug!("Client disconnected before opening session");
            return;
        }
        Err(e) => {
            warn!("Stream error during handshake: {e}");
            return;
        }
    };

    // Namespace the source key to avoid collisions with file sources
    let source = format!("session:{session_name}");

    // Process subsequent messages
    let scheduler = agent.scheduler();
    loop {
        tokio::select! {
            result = stream.message() => match result {
                Ok(Some(msg)) => {
                    let response =
                        handle_message(msg, scheduler, &source, &session_name, default_priority).await;
                    if let Some(response) = response
                        && tx.send(Ok(response)).await.is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    debug!("TVR session closed: '{session_name}'");
                    break;
                }
                Err(e) => {
                    warn!("TVR session '{session_name}' stream error: {e}");
                    break;
                }
            },
            _ = cancel.cancelled() => {
                debug!("TVR session '{session_name}' cancelled");
                break;
            }
        }
    }

    // Session ended — withdraw all contacts from this source
    info!("Withdrawing contacts for session '{session_name}'");
    metrics::gauge!("tvr_sessions").decrement(1.0);
    scheduler.withdraw_all(&source).await;
    active_sessions.lock().remove(&session_name);
}

async fn handle_message(
    msg: ClientMessage,
    scheduler: &crate::scheduler::SchedulerHandle,
    source: &str,
    session_name: &str,
    default_priority: u32,
) -> Option<ServerMessage> {
    let msg_id = msg.msg_id;

    match msg.msg {
        Some(client_message::Msg::Open(_)) => {
            warn!("TVR session '{session_name}': duplicate OpenSession");
            Some(ServerMessage {
                msg_id,
                msg: Some(server_message::Msg::Status(
                    tonic::Status::already_exists("Session already open").into(),
                )),
            })
        }
        Some(client_message::Msg::Add(req)) => {
            debug!(
                "TVR session '{session_name}': AddContacts ({} contacts)",
                req.contacts.len()
            );
            let contacts = match convert_contacts(req.contacts) {
                Ok(c) => c,
                Err(e) => {
                    return Some(ServerMessage {
                        msg_id,
                        msg: Some(server_message::Msg::Status(e.into())),
                    });
                }
            };
            let result = scheduler
                .add_contacts(source, contacts, default_priority)
                .await;
            let (added, active, skipped) = match result {
                Some(r) => (r.added, r.active, r.skipped),
                None => (0, 0, 0),
            };
            Some(ServerMessage {
                msg_id,
                msg: Some(server_message::Msg::Add(AddContactsResponse {
                    added,
                    active,
                    skipped,
                })),
            })
        }
        Some(client_message::Msg::Remove(req)) => {
            debug!(
                "TVR session '{session_name}': RemoveContacts ({} contacts)",
                req.contacts.len()
            );
            let contacts = match convert_contacts(req.contacts) {
                Ok(c) => c,
                Err(e) => {
                    return Some(ServerMessage {
                        msg_id,
                        msg: Some(server_message::Msg::Status(e.into())),
                    });
                }
            };
            let result = scheduler.remove_contacts(source, contacts).await;
            let removed = result.map(|r| r.removed).unwrap_or(0);
            Some(ServerMessage {
                msg_id,
                msg: Some(server_message::Msg::Remove(RemoveContactsResponse {
                    removed,
                })),
            })
        }
        Some(client_message::Msg::Replace(req)) => {
            debug!(
                "TVR session '{session_name}': ReplaceContacts ({} contacts)",
                req.contacts.len()
            );
            let contacts = match convert_contacts(req.contacts) {
                Ok(c) => c,
                Err(e) => {
                    return Some(ServerMessage {
                        msg_id,
                        msg: Some(server_message::Msg::Status(e.into())),
                    });
                }
            };
            let result = scheduler
                .replace_contacts(source, contacts, default_priority)
                .await;
            let (added, removed, unchanged) = match result {
                Some(r) => (r.added, r.removed, r.unchanged),
                None => (0, 0, 0),
            };
            Some(ServerMessage {
                msg_id,
                msg: Some(server_message::Msg::Replace(ReplaceContactsResponse {
                    added,
                    removed,
                    unchanged,
                })),
            })
        }
        None => {
            warn!("TVR session '{session_name}': empty message");
            None
        }
    }
}

/// Create and start the TVR gRPC server.
pub async fn start(
    listen_addr: std::net::SocketAddr,
    agent: &Arc<TvrAgent>,
    tasks: &hardy_async::TaskPool,
) {
    let service = TvrService::new(agent, tasks);
    let cancel_token = tasks.cancel_token().clone();

    hardy_async::spawn!(tasks, "tvr_grpc_server", async move {
        info!("TVR gRPC server listening on {listen_addr}");
        tonic::transport::Server::builder()
            .add_service(tvr_server::TvrServer::new(service))
            .serve_with_shutdown(listen_addr, cancel_token.cancelled())
            .await
            .expect("TVR gRPC server failed");
    });
}
