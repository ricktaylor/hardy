use crate::contacts::TvrAgent;
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

pub struct TvrService {
    agent: Arc<TvrAgent>,
}

impl TvrService {
    pub fn new(agent: &Arc<TvrAgent>) -> Self {
        Self {
            agent: agent.clone(),
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

        tokio::spawn(async move {
            run_session(stream, tx, agent).await;
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
) {
    // First message must be OpenSession
    let (session_name, default_priority) = match stream.message().await {
        Ok(Some(ClientMessage {
            msg_id,
            msg: Some(client_message::Msg::Open(open)),
        })) => {
            let name = open.name.clone();
            let priority = if open.default_priority == 0 {
                agent.default_priority()
            } else {
                open.default_priority
            };

            info!("TVR session opened: '{name}' (priority {priority})");

            let response = ServerMessage {
                msg_id,
                msg: Some(server_message::Msg::Open(OpenSessionResponse {})),
            };
            if tx.send(Ok(response)).await.is_err() {
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

    // Process subsequent messages
    let scheduler = agent.scheduler();
    loop {
        match stream.message().await {
            Ok(Some(msg)) => {
                let response =
                    handle_message(msg, scheduler, &session_name, default_priority).await;
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
        }
    }

    // Session ended — withdraw all contacts from this source
    info!("Withdrawing contacts for session '{session_name}'");
    scheduler.withdraw_all(&session_name).await;
}

async fn handle_message(
    msg: ClientMessage,
    scheduler: &crate::scheduler::SchedulerHandle,
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
            // TODO: convert proto contacts to internal Contact structs
            let contacts = Vec::new(); // placeholder
            let result = scheduler
                .add_contacts(session_name, contacts, default_priority)
                .await;
            let (added, active, skipped) = match result {
                Some(r) => (r.added, r.active, r.skipped),
                None => (0, 0, req.contacts.len() as u32),
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
            // TODO: convert proto contacts to internal Contact structs
            let contacts = Vec::new(); // placeholder
            let result = scheduler.remove_contacts(session_name, contacts).await;
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
            // TODO: convert proto contacts to internal Contact structs
            let contacts = Vec::new(); // placeholder
            let result = scheduler
                .replace_contacts(session_name, contacts, default_priority)
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
    let service = TvrService::new(agent);
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
