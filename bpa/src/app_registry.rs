use super::*;
use hardy_proto::application::*;
use rand::distributions::{Alphanumeric, DistString};
use rand::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

type Channel =
    Arc<tokio::sync::Mutex<application_client::ApplicationClient<tonic::transport::Channel>>>;

pub struct Endpoint {
    inner: Option<Channel>,
    token: String,
}

#[derive(Clone)]
struct Application {
    eid: bundle::Eid,
    token: String,
    ident: String,
    endpoint: Option<Channel>,
}

#[derive(Default, Clone)]
struct Indexed {
    applications_by_eid: HashMap<bundle::Eid, Application>,
    applications_by_token: HashMap<String, Application>,
}

#[derive(Clone)]
pub struct AppRegistry {
    node_id: node_id::NodeId,
    applications: Arc<RwLock<Indexed>>,
}

impl AppRegistry {
    pub fn new(_config: &config::Config, node_id: node_id::NodeId) -> AppRegistry {
        AppRegistry {
            node_id,
            applications: Default::default(),
        }
    }

    pub async fn register(
        &self,
        request: RegisterApplicationRequest,
    ) -> Result<RegisterApplicationResponse, tonic::Status> {
        // Compose a token first
        let token = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);

        // Connect to client gRPC address
        let endpoint = if let Some(grpc_address) = request.grpc_address {
            application_client::ApplicationClient::connect(grpc_address.clone())
                .await
                .map(|endpoint| Some(Arc::new(tokio::sync::Mutex::new(endpoint))))
                .map_err(|err| {
                    log::warn!(
                        "Failed to connect to application client at {}",
                        grpc_address
                    );
                    tonic::Status::invalid_argument(err.to_string())
                })?
        } else {
            None
        };

        let mut applications = self
            .applications
            .write()
            .log_expect("Failed to write-lock applications mutex");

        // Compose EID
        let eid = match &request.endpoint {
            Some(register_application_request::Endpoint::DtnService(s)) => {
                if s.is_empty() {
                    return Err(tonic::Status::invalid_argument(
                        "Cannot register the administrative endpoint",
                    ));
                } else if let Some(bundle::Eid::Dtn {
                    node_name,
                    demux: _,
                }) = &self.node_id.dtn
                {
                    bundle::Eid::Dtn {
                        node_name: node_name.clone(),
                        demux: s.clone(),
                    }
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
                } else if let Some(bundle::Eid::Ipn3 {
                    allocator_id,
                    node_number,
                    service_number: _,
                }) = &self.node_id.ipn
                {
                    bundle::Eid::Ipn3 {
                        allocator_id: *allocator_id,
                        node_number: *node_number,
                        service_number: *s,
                    }
                } else {
                    return Err(tonic::Status::not_found(
                        "Node does not have a ipn scheme node-number",
                    ));
                }
            }
            None => {
                let mut rng = rand::thread_rng();
                loop {
                    let eid = match (&self.node_id.ipn, &self.node_id.dtn) {
                        (
                            None,
                            Some(bundle::Eid::Dtn {
                                node_name,
                                demux: _,
                            }),
                        ) => bundle::Eid::Dtn {
                            node_name: node_name.clone(),
                            demux: format!("auto/{}", Alphanumeric.sample_string(&mut rng, 16)),
                        },
                        (
                            Some(bundle::Eid::Ipn3 {
                                allocator_id,
                                node_number,
                                service_number: _,
                            }),
                            _,
                        ) => bundle::Eid::Ipn3 {
                            allocator_id: *allocator_id,
                            node_number: *node_number,
                            service_number: (Into::<u16>::into(rng.gen::<std::num::NonZeroU16>())
                                & 0x7F7Fu16) as u32,
                        },
                        _ => unreachable!(),
                    };

                    if !applications.applications_by_eid.contains_key(&eid) {
                        break eid;
                    }
                }
            }
        };

        if request.endpoint.is_some() {
            if let Some(application) = applications.applications_by_eid.get(&eid) {
                if application.ident != request.ident {
                    return Err(tonic::Status::already_exists(format!(
                        "Endpoint {} already registered",
                        eid
                    )));
                }
            }
        }

        let response = RegisterApplicationResponse {
            token,
            endpoint_id: eid.to_string(),
        };
        let app = Application {
            eid,
            ident: request.ident,
            token: response.token.clone(),
            endpoint,
        };
        applications
            .applications_by_eid
            .insert(app.eid.clone(), app.clone());
        applications
            .applications_by_token
            .insert(app.token.clone(), app);
        Ok(response)
    }

    pub fn unregister(&self, request: UnregisterApplicationRequest) -> Result<(), tonic::Status> {
        let mut applications = self
            .applications
            .write()
            .log_expect("Failed to write-lock applications mutex");

        applications
            .applications_by_token
            .remove(&request.token)
            .and_then(|app| applications.applications_by_eid.remove(&app.eid))
            .ok_or(tonic::Status::not_found("No such application registered"))
            .map(|_| ())
    }

    pub fn lookup_by_token(&self, token: &str) -> Result<bundle::Eid, tonic::Status> {
        self.applications
            .read()
            .log_expect("Failed to read-lock applications mutex")
            .applications_by_token
            .get(token)
            .ok_or(tonic::Status::not_found("No such application"))
            .map(|app| app.eid.clone())
    }

    pub fn lookup_by_eid(&self, eid: &bundle::Eid) -> Option<Endpoint> {
        self.applications
            .read()
            .log_expect("Failed to read-lock applications mutex")
            .applications_by_eid
            .get(eid)
            .map(|app| Endpoint {
                token: app.token.clone(),
                inner: app.endpoint.clone(),
            })
    }
}

impl Endpoint {
    pub async fn collection_notify(&self, bundle_id: &bundle::BundleId) {
        if let Some(endpoint) = &self.inner {
            let _ = endpoint
                .lock()
                .await
                .collection_notify(tonic::Request::new(CollectionNotifyRequest {
                    token: self.token.clone(),
                    bundle_id: bundle_id.to_key(),
                }))
                .await;
        }
    }
}