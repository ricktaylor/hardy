use super::*;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

pub struct Bpa {
    //store: Arc<store::Store>,
    fib: Arc<fib_impl::Fib>,
    cla_registry: Arc<cla_registry::ClaRegistry>,
    service_registry: Arc<service_registry::ServiceRegistry>,
    dispatcher: Arc<dispatcher::Dispatcher>,
}

impl Bpa {
    #[instrument]
    pub async fn start(config: &config::Config) -> Self {
        trace!("Starting new BPA");

        // New store
        let store = Arc::new(store::Store::new(config));

        // New FIB
        let fib = Arc::new(fib_impl::Fib::new());

        // New registries
        let cla_registry = Arc::new(cla_registry::ClaRegistry::new(fib.clone()));
        let service_registry = Arc::new(service_registry::ServiceRegistry::new(config));

        // Create a new dispatcher
        let (dispatcher, rx) = dispatcher::Dispatcher::new(
            config,
            store.clone(),
            service_registry.clone(),
            fib.clone(),
        );
        let dispatcher = Arc::new(dispatcher);

        // Spawn the dispatch task
        tokio::spawn(dispatcher::Dispatcher::run(dispatcher.clone(), rx));

        trace!("BPA started");

        Self {
            //store,
            fib,
            cla_registry,
            service_registry,
            dispatcher,
        }
    }

    #[instrument(skip(self))]
    pub async fn shutdown(&self) {
        trace!("Shutting down BPA");

        self.dispatcher.shutdown().await;
        self.service_registry.shutdown().await;
        self.cla_registry.shutdown().await;

        trace!("BPA stopped");
    }

    #[instrument(skip(self, service))]
    pub async fn register_service(
        &self,
        eid: Option<&service::ServiceName<'_>>,
        service: Arc<dyn service::Service>,
    ) -> service::Result<()> {
        self.service_registry
            .register(eid, service, self.dispatcher.clone())
            .await
    }

    #[instrument(skip(self, cla))]
    pub async fn register_cla(
        &self,
        ident: &str,
        kind: &str,
        cla: Arc<dyn cla::Cla>,
    ) -> cla::Result<()> {
        self.cla_registry
            .register(ident, kind, cla, self.dispatcher.clone())
            .await
    }

    pub async fn add_forwarding_action(
        &self,
        id: &str,
        pattern: &bpv7::EidPattern,
        action: &fib::Action,
        priority: u32,
    ) -> fib::Result<()> {
        self.fib.add(id, pattern, action, priority).await
    }

    pub async fn remove_forwarding_action(&self, id: &str, pattern: &bpv7::EidPattern) -> usize {
        self.fib.remove(id, pattern).await
    }
}
