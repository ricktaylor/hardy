use super::*;
use hardy_proto::cla::*;
use rand::Rng;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};
use tokio_util::bytes::Bytes;

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

/*#[derive(Clone)]
struct Config {
}

impl Config {
    fn new(config: &config::Config) -> Self {
        Self {
        }
    }
}*/

#[derive(Clone)]
pub struct ClaRegistry {
    //config: Config,
    clas: Arc<RwLock<HashMap<u32, Arc<Cla>>>>,
    fib: Option<fib::Fib>,
}

impl ClaRegistry {
    pub fn new(_config: &config::Config, fib: Option<fib::Fib>) -> Self {
        Self {
            //config: Config::new(config),
            fib,
            clas: Arc::new(RwLock::new(HashMap::new())),
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

        info!("Registered new CLA: {}/{}", request.name, request.ident);

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
            .map(|cla| {
                info!("Unregistered CLA: {}/{}", cla.name, cla.ident);
                UnregisterClaResponse {}
            })
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

    #[instrument(skip(self))]
    pub async fn add_neighbour(&self, request: AddNeighbourRequest) -> Result<(), tonic::Status> {
        let cla = self
            .clas
            .read()
            .await
            .get(&request.handle)
            .ok_or(tonic::Status::not_found("No such CLA registered"))?
            .clone();

        let Some(fib) = &self.fib else {
            return Ok(());
        };

        let neighbour = request
            .neighbour
            .parse::<bpv7::EidPattern>()
            .map_err(|e| tonic::Status::invalid_argument(e.to_string()))?;

        fib.add(
            format!("cla:{}", cla.name),
            &neighbour,
            request.priority,
            fib::Action::Forward(fib::Endpoint {
                handle: request.handle,
            }),
        )
        .await
        .map_err(tonic::Status::from_error)
    }

    #[instrument(skip(self))]
    pub async fn remove_neighbour(
        &self,
        request: RemoveNeighbourRequest,
    ) -> Result<(), tonic::Status> {
        let cla = self
            .clas
            .read()
            .await
            .get(&request.handle)
            .ok_or(tonic::Status::not_found("No such CLA registered"))?
            .clone();

        let Some(fib) = &self.fib else {
            return Err(tonic::Status::not_found("No such neighbour"));
        };

        let neighbour = request
            .neighbour
            .parse::<bpv7::EidPattern>()
            .map_err(|e| tonic::Status::invalid_argument(e.to_string()))?;

        if fib
            .remove(&format!("cla:{}", cla.name), &neighbour)
            .await
            .is_none()
        {
            Err(tonic::Status::not_found("No such neighbour"))
        } else {
            Ok(())
        }
    }
}

pub enum ForwardBundleResult {
    Sent,
    Pending(u32, Option<time::OffsetDateTime>),
    Congested(time::OffsetDateTime),
}

impl Endpoint {
    #[instrument(skip(self))]
    pub async fn forward_bundle(
        &self,
        destination: &bpv7::Eid,
        bundle: Bytes,
    ) -> Result<ForwardBundleResult, Error> {
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
            Some(grpc::from_timestamp(t)?)
        } else {
            None
        };

        // This is just horrible
        match r.result {
            v if v == (forward_bundle_response::ForwardingResult::Sent as i32) => {
                Ok(ForwardBundleResult::Sent)
            }
            v if v == (forward_bundle_response::ForwardingResult::Pending as i32) => {
                Ok(ForwardBundleResult::Pending(self.handle, delay))
            }
            v if v == (forward_bundle_response::ForwardingResult::Congested as i32) => {
                if let Some(delay) = delay {
                    Ok(ForwardBundleResult::Congested(delay))
                } else {
                    Ok(ForwardBundleResult::Congested(
                        time::OffsetDateTime::now_utc(),
                    ))
                }
            }
            v => {
                Err(tonic::Status::invalid_argument(format!("Invalid result {v} received")).into())
            }
        }
    }
}
