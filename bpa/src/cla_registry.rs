use super::*;
use hardy_proto::cla::*;
use rand::distributions::{Alphanumeric, DistString};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

struct Cla {
    name: String,
    protocol: String,
    endpoint: Arc<tokio::sync::Mutex<cla_client::ClaClient<tonic::transport::Channel>>>,
}

#[derive(Default, Clone)]
pub struct ClaRegistry {
    clas: Arc<RwLock<HashMap<String, Cla>>>,
}

impl ClaRegistry {
    pub fn new(_config: &config::Config) -> Self {
        Self {
            ..Default::default()
        }
    }

    pub async fn register(
        &self,
        request: RegisterClaRequest,
    ) -> Result<RegisterClaResponse, tonic::Status> {
        // Connect to client gRPC address
        let endpoint = Arc::new(tokio::sync::Mutex::new(
            cla_client::ClaClient::connect(request.grpc_address.clone())
                .await
                .map_err(|e| {
                    log::warn!(
                        "Failed to connect to CLA client at {}",
                        request.grpc_address
                    );
                    tonic::Status::invalid_argument(e.to_string())
                })?,
        ));

        // Compose a token
        let mut rng = rand::thread_rng();
        let mut token = Alphanumeric.sample_string(&mut rng, 16);

        let mut clas = self
            .clas
            .write()
            .log_expect("Failed to write-lock CLA mutex");

        // Check token is unique
        while clas.contains_key(&token) {
            token = Alphanumeric.sample_string(&mut rng, 16);
        }

        // Do a linear search for re-registration with the same name
        for (k, cla) in clas.iter_mut() {
            if cla.name == request.name {
                cla.endpoint = endpoint;
                return Ok(RegisterClaResponse { token: k.clone() });
            }
        }

        clas.insert(
            token.clone(),
            Cla {
                protocol: request.protocol,
                name: request.name,
                endpoint,
            },
        );
        Ok(RegisterClaResponse { token })
    }

    pub fn unregister(
        &self,
        request: UnregisterClaRequest,
    ) -> Result<UnregisterClaResponse, tonic::Status> {
        self.clas
            .write()
            .log_expect("Failed to write-lock CLA mutex")
            .remove(&request.token)
            .ok_or(tonic::Status::not_found("No such CLA registered"))
            .map(|_| UnregisterClaResponse {})
    }

    pub async fn forward_bundle(
        &self,
        request: ForwardBundleRequest,
    ) -> Result<bool, tonic::Status> {
        {
            // Scope the read-lock
            let clas = self.clas.read().log_expect("Failed to read-lock CLA mutex");
            match clas.get(&request.token) {
                None => return Ok(false),
                Some(cla) => cla.endpoint.clone(),
            }
        }
        .lock()
        .await
        .forward_bundle(tonic::Request::new(request))
        .await
        .inspect_err(|e| log::warn!("Failed to forward bundle: {}", e))
        .map(|_| true)
    }

    pub fn lookup(&self, token: &str) -> Result<(String, String), tonic::Status> {
        self.clas
            .read()
            .log_expect("Failed to read-lock CLA mutex")
            .get(token)
            .ok_or(tonic::Status::not_found("No such CLA registered"))
            .map(|cla| (cla.protocol.clone(), cla.name.clone()))
    }
}
