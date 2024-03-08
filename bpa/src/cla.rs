use super::*;
use hardy_proto::bpa::*;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug)]
struct Cla {
    ident: String,
    endpoint:
        Arc<tokio::sync::Mutex<hardy_proto::bpa::cla_client::ClaClient<tonic::transport::Channel>>>,
}

#[derive(Debug)]
struct ClaRegistryInner {
    clas: HashMap<String, Cla>,
}

#[derive(Debug)]
pub struct ClaRegistry {
    inner: RwLock<ClaRegistryInner>,
}

impl ClaRegistry {
    pub fn new(_config: &settings::Config) -> ClaRegistry {
        ClaRegistry {
            inner: RwLock::new(ClaRegistryInner {
                clas: HashMap::new(),
            }),
        }
    }

    pub async fn register(&self, request: RegisterClaRequest) -> Result<(), tonic::Status> {
        {
            // Scope the read-lock
            let inner = self
                .inner
                .read()
                .log_expect("Failed to read-lock CLA mutex");
            if let Some(cla) = inner.clas.get(&request.protocol) {
                if cla.ident != request.ident {
                    return Err(tonic::Status::already_exists(format!(
                        "CLA for protocol {} already registered",
                        request.protocol
                    )));
                }
            }
        }

        let endpoint = cla_client::ClaClient::connect(request.grpc_address.clone())
            .await
            .map_err(|e| {
                log::warn!(
                    "Failed to connect to to CLA client at {}",
                    request.grpc_address
                );
                tonic::Status::invalid_argument(e.to_string())
            })?;

        let mut inner = self
            .inner
            .write()
            .log_expect("Failed to write-lock CLA mutex");
        if let Some(cla) = inner.clas.get(&request.protocol) {
            // Check for races
            if cla.ident != request.ident {
                return Err(tonic::Status::already_exists(format!(
                    "CLA for protocol {} already registered",
                    request.protocol
                )));
            }
        }

        inner.clas.insert(
            request.protocol,
            Cla {
                ident: request.ident,
                endpoint: Arc::new(tokio::sync::Mutex::new(endpoint)),
            },
        );
        Ok(())
    }

    pub fn unregister(&self, request: UnregisterClaRequest) -> Result<(), tonic::Status> {
        let mut inner = self
            .inner
            .write()
            .log_expect("Failed to write-lock CLA mutex");
        if let Some(cla) = inner.clas.get(&request.protocol) {
            if cla.ident == request.ident {
                // Matching ident
                inner.clas.remove(&request.protocol);
                return Ok(());
            }
        }
        Err(tonic::Status::not_found("No such CLA registered"))
    }

    pub async fn forward_bundle(&self, request: ForwardBundleRequest) -> bool {
        {
            // Scope the read-lock
            let inner = self
                .inner
                .read()
                .log_expect("Failed to read-lock CLA mutex");
            match inner.clas.get(&request.protocol) {
                None => return false,
                Some(cla) => cla.endpoint.clone(),
            }
        }
        .lock()
        .await
        .forward_bundle(tonic::Request::new(request))
        .await
        .is_err()
    }
}
