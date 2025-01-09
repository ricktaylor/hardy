use super::*;
use hardy_proto::application::*;
use rand::distributions::{Alphanumeric, DistString};
use rand::Rng;
use std::collections::HashMap;
use tokio::sync::{Mutex, RwLock};

type Channel = Arc<Mutex<application_client::ApplicationClient<tonic::transport::Channel>>>;

pub struct Endpoint {
    inner: Option<Channel>,
    token: String,
}

#[derive(Debug)]
pub enum StatusKind {
    Received = 1,
    Forwarded = 2,
    Delivered = 3,
    Deleted = 4,
}

struct Application {
    eid: bpv7::Eid,
    token: String,
    ident: String,
    endpoint: Option<Channel>,
}

#[derive(Default)]
struct Indexes {
    applications_by_eid: HashMap<bpv7::Eid, Arc<Application>>,
    applications_by_token: HashMap<String, Arc<Application>>,
}

#[derive(Clone)]
pub struct AppRegistry {
    admin_endpoints: utils::admin_endpoints::AdminEndpoints,
    applications: Arc<RwLock<Indexes>>,
}

impl AppRegistry {
    pub fn new(
        _config: &config::Config,
        admin_endpoints: utils::admin_endpoints::AdminEndpoints,
    ) -> Self {
        Self {
            admin_endpoints,
            applications: Default::default(),
        }
    }

    #[instrument(skip(self))]
    pub async fn register(
        &self,
        request: RegisterApplicationRequest,
    ) -> Result<RegisterApplicationResponse, tonic::Status> {
        // Connect to client gRPC address
        let endpoint = if let Some(grpc_address) = request.grpc_address {
            application_client::ApplicationClient::connect(grpc_address.clone())
                .await
                .map(|endpoint| Some(Arc::new(Mutex::new(endpoint))))
                .map_err(|e| {
                    warn!("Failed to connect to application client at {grpc_address}: {e}");
                    tonic::Status::invalid_argument(e.to_string())
                })?
        } else {
            None
        };

        // Compose a token
        let mut token = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
        let mut applications = self.applications.write().await;

        // Check token is unique
        while applications.applications_by_token.contains_key(&token) {
            token = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
        }

        // Compose EID
        let eid = match &request.endpoint {
            Some(register_application_request::Endpoint::DtnService(s)) => {
                if s.is_empty() {
                    return Err(tonic::Status::invalid_argument(
                        "Cannot register the administrative endpoint",
                    ));
                } else if let Some(node_id) = &self.admin_endpoints.dtn {
                    node_id
                        .to_eid(s)
                        .map_err(|e| tonic::Status::invalid_argument(e.to_string()))?
                } else {
                    return Err(tonic::Status::not_found(
                        "Node does not have a dtn scheme node-name",
                    ));
                }
            }
            Some(register_application_request::Endpoint::IpnServiceNumber(s)) => {
                if *s == 0 {
                    return Err(tonic::Status::invalid_argument(
                        "Cannot register the administrative endpoint",
                    ));
                } else if let Some(node_id) = &self.admin_endpoints.ipn {
                    node_id.to_eid(*s)
                } else {
                    return Err(tonic::Status::not_found(
                        "Node does not have a ipn scheme fully-qualified node-number",
                    ));
                }
            }
            None => loop {
                let eid = match (&self.admin_endpoints.ipn, &self.admin_endpoints.dtn) {
                    (None, Some(node_id)) => node_id
                        .to_eid(&format!(
                            "auto/{}",
                            Alphanumeric.sample_string(&mut rand::thread_rng(), 16)
                        ))
                        .map_err(|e| tonic::Status::invalid_argument(e.to_string()))?,
                    (Some(node_id), _) => node_id.to_eid(
                        (Into::<u16>::into(rand::thread_rng().gen::<std::num::NonZeroU16>())
                            & 0x7F7Fu16) as u32,
                    ),
                    _ => unreachable!(),
                };

                if !applications.applications_by_eid.contains_key(&eid) {
                    break eid;
                }
            },
        };

        if request.endpoint.is_some() {
            if let Some(application) = applications.applications_by_eid.get(&eid) {
                if application.ident != request.ident {
                    return Err(tonic::Status::already_exists(format!(
                        "Endpoint {eid} already registered"
                    )));
                }
            }
        }

        let response = RegisterApplicationResponse {
            token,
            endpoint_id: eid.to_string(),
        };
        let app = Arc::new(Application {
            eid,
            ident: request.ident,
            token: response.token.clone(),
            endpoint,
        });
        applications
            .applications_by_eid
            .insert(app.eid.clone(), app.clone());
        applications
            .applications_by_token
            .insert(app.token.clone(), app);
        Ok(response)
    }

    #[instrument(skip(self))]
    pub async fn unregister(
        &self,
        request: UnregisterApplicationRequest,
    ) -> Result<UnregisterApplicationResponse, tonic::Status> {
        let mut applications = self.applications.write().await;

        applications
            .applications_by_token
            .remove(&request.token)
            .and_then(|app| applications.applications_by_eid.remove(&app.eid))
            .ok_or(tonic::Status::not_found("No such application registered"))
            .map(|_| UnregisterApplicationResponse {})
    }

    #[instrument(skip(self))]
    pub async fn find_by_token(&self, token: &str) -> Result<bpv7::Eid, tonic::Status> {
        self.applications
            .read()
            .await
            .applications_by_token
            .get(token)
            .ok_or(tonic::Status::not_found("No such application"))
            .map(|app| app.eid.clone())
    }

    #[instrument(skip(self))]
    pub async fn find_by_eid(&self, eid: &bpv7::Eid) -> Option<Endpoint> {
        self.applications
            .read()
            .await
            .applications_by_eid
            .get(eid)
            .map(|app| Endpoint {
                token: app.token.clone(),
                inner: app.endpoint.clone(),
            })
    }
}

impl Endpoint {
    #[instrument(skip(self))]
    pub async fn collection_notify(&self, bundle_id: &bpv7::BundleId) {
        if let Some(endpoint) = &self.inner {
            _ = endpoint
                .lock()
                .await
                .collection_notify(tonic::Request::new(CollectionNotifyRequest {
                    token: self.token.clone(),
                    bundle_id: bundle_id.to_key(),
                }))
                .await
                .inspect_err(|s| info!("collection_notify failed: {s}"));
        }
    }

    #[instrument(skip(self))]
    pub async fn status_notify(
        &self,
        bundle_id: &bpv7::BundleId,
        kind: StatusKind,
        reason: bpv7::StatusReportReasonCode,
        timestamp: Option<time::OffsetDateTime>,
    ) {
        if let Some(endpoint) = &self.inner {
            _ = endpoint
                .lock()
                .await
                .status_notify(tonic::Request::new(StatusNotifyRequest {
                    token: self.token.clone(),
                    bundle_id: bundle_id.to_key(),
                    kind: kind as i32,
                    reason: reason.into(),
                    timestamp: timestamp.map(grpc::to_timestamp),
                }))
                .await
                .inspect_err(|s| info!("status_notify failed: {s}"));
        }
    }
}
