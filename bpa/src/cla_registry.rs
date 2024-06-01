use super::*;
use hardy_proto::cla::*;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

type Channel = Arc<Mutex<cla_client::ClaClient<tonic::transport::Channel>>>;

pub struct Endpoint {
    inner: Channel,
    handle: u32,
}

struct Cla {
    ident: String,
    name: String,
    endpoint: Channel,
}

#[derive(Default, Clone)]
pub struct ClaRegistry {
    clas: Arc<RwLock<HashMap<u32, Arc<Cla>>>>,
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
        let endpoint = Arc::new(Mutex::new(
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

        let mut clas = self.clas.write().await;

        // Compose a handle
        let mut rng = rand::thread_rng();
        let mut handle = rng.gen::<std::num::NonZeroU32>().into();

        // Check handle is unique
        while clas.contains_key(&handle) {
            handle = rng.gen::<std::num::NonZeroU32>().into();
        }

        // Do a linear search for re-registration with the same name
        for (_, cla) in clas.iter_mut() {
            if cla.ident != request.ident {
                return Err(tonic::Status::already_exists(format!(
                    "CLA {} already registered",
                    request.ident
                )));
            }
        }

        let cla = Arc::new(Cla {
            ident: request.ident,
            name: request.name,
            endpoint,
        });

        clas.insert(handle, cla.clone());
        Ok(RegisterClaResponse { handle })
    }

    #[instrument(skip(self))]
    pub async fn unregister(
        &self,
        request: UnregisterClaRequest,
    ) -> Result<UnregisterClaResponse, tonic::Status> {
        let mut clas = self.clas.write().await;

        clas.remove(&request.handle)
            .ok_or(tonic::Status::not_found("No such CLA registered"))
            .map(|_| UnregisterClaResponse {})
    }

    #[instrument(skip(self))]
    pub async fn exists(&self, handle: u32) -> Result<(), tonic::Status> {
        if !self.clas.read().await.contains_key(&handle) {
            Err(tonic::Status::not_found("No such CLA registered"))
        } else {
            Ok(())
        }
    }

    #[instrument(skip(self))]
    pub async fn find(&self, handle: u32) -> Option<Endpoint> {
        self.clas.read().await.get(&handle).map(|cla| Endpoint {
            handle,
            inner: cla.endpoint.clone(),
        })
    }
}

impl Endpoint {
    #[instrument(skip(self))]
    pub async fn forward_bundle(
        &self,
        destination: &bundle::Eid,
        bundle: Vec<u8>,
    ) -> Result<(Option<u32>, Option<time::OffsetDateTime>), Error> {
        let r = self
            .inner
            .lock()
            .await
            .forward_bundle(tonic::Request::new(ForwardBundleRequest {
                handle: self.handle,
                destination: destination.to_string(),
                bundle,
            }))
            .await?
            .into_inner();

        let delay = if let Some(t) = r.delay {
            let delay = services::from_timestamp(t)?;
            if delay <= time::OffsetDateTime::now_utc() {
                None
            } else {
                Some(delay)
            }
        } else {
            None
        };

        // This is just horrible
        match r.result {
            v if v == (forward_bundle_response::ForwardingResult::Sent as i32) => Ok((None, None)),
            v if v == (forward_bundle_response::ForwardingResult::Pending as i32) => {
                Ok((Some(self.handle), delay))
            }
            v if v == (forward_bundle_response::ForwardingResult::Congested as i32) => {
                Ok((None, delay))
            }
            v => {
                Err(tonic::Status::invalid_argument(format!("Invalid result {v} received")).into())
            }
        }
    }
}
