use super::*;
use application_sink_server::{ApplicationSink, ApplicationSinkServer};
use hardy_proto::application::*;
use tonic::{Request, Response, Status};

pub struct Service {
    app_registry: app_registry::AppRegistry,
    dispatcher: dispatcher::Dispatcher,
}

impl Service {
    fn new(
        _config: &config::Config,
        app_registry: app_registry::AppRegistry,
        dispatcher: dispatcher::Dispatcher,
    ) -> Self {
        Service {
            app_registry,
            dispatcher,
        }
    }
}

#[tonic::async_trait]
impl ApplicationSink for Service {
    #[instrument(skip(self))]
    async fn register_application(
        &self,
        request: Request<RegisterApplicationRequest>,
    ) -> Result<Response<RegisterApplicationResponse>, Status> {
        self.app_registry
            .register(request.into_inner())
            .await
            .map(Response::new)
    }

    #[instrument(skip(self))]
    async fn unregister_application(
        &self,
        request: Request<UnregisterApplicationRequest>,
    ) -> Result<Response<UnregisterApplicationResponse>, Status> {
        self.app_registry
            .unregister(request.into_inner())
            .map(Response::new)
    }

    #[instrument(skip(self))]
    async fn send(&self, request: Request<SendRequest>) -> Result<Response<SendResponse>, Status> {
        let request = request.into_inner();
        let eid = self.app_registry.find_by_token(&request.token)?;
        self.dispatcher
            .local_dispatch(eid, request)
            .await
            .map(|_| Response::new(SendResponse {}))
            .map_err(|e| Status::from_error(e.into()))
    }
}

pub fn new_service(
    config: &config::Config,
    app_registry: app_registry::AppRegistry,
    dispatcher: dispatcher::Dispatcher,
) -> ApplicationSinkServer<Service> {
    ApplicationSinkServer::new(Service::new(config, app_registry, dispatcher))
}
