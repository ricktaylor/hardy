use super::*;
use hardy_proto::application::*;
use rand::distributions::{Alphanumeric, DistString};
use rand::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

struct Application {
    eid: bundle::Eid,
    ident: String,
    token: String,
    endpoint:
        Arc<tokio::sync::Mutex<application_client::ApplicationClient<tonic::transport::Channel>>>,
}

#[derive(Default, Clone)]
struct Indexed {
    applications_by_eid: HashMap<bundle::Eid, Arc<Application>>,
    applications_by_token: HashMap<String, Arc<Application>>,
}

#[derive(Default, Clone)]
pub struct AppRegistry {
    administrative_endpoint: bundle::Eid,
    applications: Arc<RwLock<Indexed>>,
}

impl AppRegistry {
    pub fn new(_config: &config::Config, administrative_endpoint: bundle::Eid) -> AppRegistry {
        AppRegistry {
            administrative_endpoint,
            ..Default::default()
        }
    }

    pub async fn register(
        &self,
        request: RegisterApplicationRequest,
    ) -> Result<RegisterApplicationResponse, tonic::Status> {
        // Connect to client gRPC address
        let endpoint = application_client::ApplicationClient::connect(request.grpc_address.clone())
            .await
            .map_err(|e| {
                log::warn!(
                    "Failed to connect to application client at {}",
                    request.grpc_address
                );
                tonic::Status::invalid_argument(e.to_string())
            })?;

        let mut rng = rand::thread_rng();

        let mut applications = self
            .applications
            .write()
            .log_expect("Failed to write-lock applications mutex");

        // Compose endpoint
        let eid = match &request.endpoint {
            Some(register_application_request::Endpoint::Id(s)) => {
                let eid = s.parse::<bundle::Eid>().map_err(|e| {
                    tonic::Status::invalid_argument(format!(
                        "Failed to parse Endpoint Id '{}': {}",
                        s, e
                    ))
                })?;
                match &eid {
                    bundle::Eid::Null => unreachable!(),
                    bundle::Eid::LocalNode { service_number: _ } => unreachable!(),
                    bundle::Eid::Ipn2 {
                        allocator_id: _,
                        node_number: _,
                        service_number,
                    }
                    | bundle::Eid::Ipn3 {
                        allocator_id: _,
                        node_number: _,
                        service_number,
                    } => {
                        if *service_number == 0 {
                            return Err(tonic::Status::invalid_argument(
                                "Cannot register the administrative endpoint".to_string(),
                            ));
                        } else {
                            eid
                        }
                    }
                    bundle::Eid::Dtn {
                        node_name: _,
                        demux,
                    } => {
                        if demux.is_empty() {
                            return Err(tonic::Status::invalid_argument(
                                "Cannot register the administrative endpoint".to_string(),
                            ));
                        } else {
                            eid
                        }
                    }
                }
            }
            Some(register_application_request::Endpoint::ServiceNumber(s)) => {
                match &self.administrative_endpoint {
                    bundle::Eid::Null => unreachable!(),
                    bundle::Eid::LocalNode { service_number: _ } => unreachable!(),
                    bundle::Eid::Ipn2 {
                        allocator_id,
                        node_number,
                        service_number: _,
                    }
                    | bundle::Eid::Ipn3 {
                        allocator_id,
                        node_number,
                        service_number: _,
                    } => {
                        if *s == 0 {
                            return Err(tonic::Status::invalid_argument(
                                "Cannot register the administrative endpoint".to_string(),
                            ));
                        } else {
                            bundle::Eid::Ipn3 {
                                allocator_id: *allocator_id,
                                node_number: *node_number,
                                service_number: *s,
                            }
                        }
                    }
                    bundle::Eid::Dtn {
                        node_name,
                        demux: _,
                    } => bundle::Eid::Dtn {
                        node_name: node_name.clone(),
                        demux: format!("service{s}"),
                    },
                }
            }
            None => loop {
                let eid = match &self.administrative_endpoint {
                    bundle::Eid::Null => unreachable!(),
                    bundle::Eid::LocalNode { service_number: _ } => unreachable!(),
                    bundle::Eid::Ipn2 {
                        allocator_id,
                        node_number,
                        service_number: _,
                    }
                    | bundle::Eid::Ipn3 {
                        allocator_id,
                        node_number,
                        service_number: _,
                    } => bundle::Eid::Ipn3 {
                        allocator_id: *allocator_id,
                        node_number: *node_number,
                        service_number: (Into::<u16>::into(rng.gen::<std::num::NonZeroU16>())
                            & 0x7F7Fu16) as u32,
                    },
                    bundle::Eid::Dtn {
                        node_name,
                        demux: _,
                    } => bundle::Eid::Dtn {
                        node_name: node_name.clone(),
                        demux: Alphanumeric.sample_string(&mut rng, 8),
                    },
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
                        "Endpoint {} already registered",
                        eid
                    )));
                }
            }
        }

        let response = RegisterApplicationResponse {
            token: Alphanumeric.sample_string(&mut rng, 16),
            endpoint_id: eid.to_string(),
        };
        let app = Arc::new(Application {
            eid,
            ident: request.ident,
            token: response.token.clone(),
            endpoint: Arc::new(tokio::sync::Mutex::new(endpoint)),
        });
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

    pub async fn deliver_bundle(
        &self,
        source: &bundle::Eid,
        destination: &bundle::Eid,
        data: Vec<u8>,
    ) -> Result<bool, tonic::Status> {
        {
            // Scope the read-lock
            let applications = self
                .applications
                .read()
                .log_expect("Failed to read-lock applications mutex");
            match applications.applications_by_eid.get(destination) {
                None => return Ok(false),
                Some(application) => application.endpoint.clone(),
            }
        }
        .lock()
        .await
        .receive(tonic::Request::new(ReceiveRequest {
            source_eid: source.to_string(),
            data,
        }))
        .await
        .inspect_err(|e| log::warn!("Failed to deliver bundle: {}", e))
        .map(|_| true)
    }

    pub fn lookup_eid(&self, token: &str) -> Result<bundle::Eid, tonic::Status> {
        self.applications
            .read()
            .log_expect("Failed to read-lock applications mutex")
            .applications_by_token
            .get(token)
            .ok_or(tonic::Status::not_found("No such application"))
            .map(|app| app.eid.clone())
    }
}
