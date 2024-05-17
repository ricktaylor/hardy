use super::*;
use hardy_proto::cla::*;
use rand::distributions::{Alphanumeric, DistString};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

type Channel = Arc<tokio::sync::Mutex<cla_client::ClaClient<tonic::transport::Channel>>>;

pub struct Endpoint {
    inner: Channel,
    token: String,
}

struct Cla {
    token: String,
    name: String,
    ident: String,
    protocol: String,
    endpoint: Channel,
}

#[derive(Default)]
struct Indexes {
    clas_by_name: HashMap<String, Arc<Cla>>,
    clas_by_token: HashMap<String, Arc<Cla>>,
}

#[derive(Default, Clone)]
pub struct ClaRegistry {
    clas: Arc<RwLock<Indexes>>,
}

impl ClaRegistry {
    pub fn new(_config: &config::Config) -> Self {
        Self {
            ..Default::default()
        }
    }

    #[instrument(skip(self))]
    pub async fn register(
        &self,
        request: RegisterClaRequest,
    ) -> Result<RegisterClaResponse, tonic::Status> {
        // Connect to client gRPC address
        let endpoint = Arc::new(tokio::sync::Mutex::new(
            cla_client::ClaClient::connect(request.grpc_address.clone())
                .await
                .map_err(|e| {
                    warn!(
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
            .trace_expect("Failed to write-lock CLA mutex");

        // Check token is unique
        while clas.clas_by_token.contains_key(&token) {
            token = Alphanumeric.sample_string(&mut rng, 16);
        }

        // Do a linear search for re-registration with the same name
        for (_, cla) in clas.clas_by_token.iter_mut() {
            if cla.ident != request.ident {
                return Err(tonic::Status::already_exists(format!(
                    "CLA {} already registered",
                    request.ident
                )));
            }
        }

        let cla = Arc::new(Cla {
            token: token.clone(),
            name: request.name.clone(),
            ident: request.ident,
            protocol: request.protocol,
            endpoint,
        });

        clas.clas_by_token.insert(token.clone(), cla.clone());
        clas.clas_by_name.insert(request.name, cla);
        Ok(RegisterClaResponse { token })
    }

    #[instrument(skip(self))]
    pub fn unregister(
        &self,
        request: UnregisterClaRequest,
    ) -> Result<UnregisterClaResponse, tonic::Status> {
        let mut clas = self
            .clas
            .write()
            .trace_expect("Failed to write-lock CLA mutex");

        clas.clas_by_token
            .remove(&request.token)
            .and_then(|cla| clas.clas_by_name.remove(&cla.name))
            .ok_or(tonic::Status::not_found("No such CLA registered"))
            .map(|_| UnregisterClaResponse {})
    }

    #[instrument(skip(self))]
    pub fn find_by_token(&self, token: &str) -> Result<(String, String), tonic::Status> {
        self.clas
            .read()
            .trace_expect("Failed to read-lock CLA mutex")
            .clas_by_token
            .get(token)
            .ok_or(tonic::Status::not_found("No such CLA registered"))
            .map(|cla| (cla.protocol.clone(), cla.name.clone()))
    }

    #[instrument(skip(self))]
    pub fn find_by_name(&self, name: &str) -> Option<Endpoint> {
        self.clas
            .read()
            .trace_expect("Failed to read-lock CLA mutex")
            .clas_by_name
            .get(name)
            .map(|cla| Endpoint {
                token: cla.token.clone(),
                inner: cla.endpoint.clone(),
            })
    }
}

impl Endpoint {
    #[instrument(skip(self))]
    pub async fn forward_bundle(
        &self,
        destination: Vec<u8>,
        bundle: Vec<u8>,
    ) -> Result<(String, Option<time::OffsetDateTime>), Error> {
        match self
            .inner
            .lock()
            .await
            .forward_bundle(tonic::Request::new(ForwardBundleRequest {
                token: self.token.clone(),
                destination,
                bundle,
            }))
            .await
            .map(|response| response.into_inner())
        {
            Err(s) => Err(s.into()),
            Ok(r) => {
                if let Some(t) = r.retry_at {
                    Ok(Some(
                        time::OffsetDateTime::from_unix_timestamp(t.seconds)?
                            + time::Duration::nanoseconds(t.nanos.into()),
                    ))
                } else {
                    Ok(None)
                }
            }
        }
    }
}
