mod app_registry;
mod cla_registry;
mod dispatcher;
mod fib;
mod ingress;
mod services;
mod static_routes;
mod store;
mod utils;

// This is the generic Error type used almost everywhere
type Error = Box<dyn std::error::Error + Send + Sync>;

// This is the effective prelude
use hardy_bpa_api::metadata;
use hardy_bpv7::prelude as bpv7;
use trace_err::*;
use tracing::{debug, error, info, instrument, trace, warn};

#[tokio::main]
async fn main() {
    // Parse command line
    let Some((config, upgrade, config_source)) = utils::settings::init() else {
        return;
    };

    // Init logger
    utils::logger::init(&config);
    info!(
        "{} version {} starting...",
        utils::built_info::PKG_NAME,
        utils::built_info::PKG_VERSION
    );
    info!("{config_source}");

    // Get administrative endpoints
    let administrative_endpoints = utils::admin_endpoints::AdminEndpoints::init(&config);

    // New store
    let store = store::Store::new(&config, upgrade);

    // New FIB
    let fib = fib::Fib::new(&config);

    // New registries
    let cla_registry = cla_registry::ClaRegistry::new(&config, fib.clone());
    let app_registry = app_registry::AppRegistry::new(&config, administrative_endpoints.clone());

    // Prepare for graceful shutdown
    let (mut task_set, cancel_token) = utils::cancel::new_cancellable_set();

    // Load static routes
    if let Some(fib) = &fib {
        static_routes::init(&config, fib.clone(), &mut task_set, cancel_token.clone()).await;
    }

    // Create a new dispatcher
    let dispatcher = dispatcher::Dispatcher::new(
        &config,
        administrative_endpoints,
        store.clone(),
        cla_registry.clone(),
        app_registry.clone(),
        fib,
        &mut task_set,
        cancel_token.clone(),
    );

    // Create a new ingress
    let ingress = ingress::Ingress::new(&config, store.clone(), dispatcher.clone());

    // Start the store - this can take a while as the store is walked
    store
        .start(
            ingress.clone(),
            dispatcher.clone(),
            &mut task_set,
            cancel_token.clone(),
        )
        .await;

    if !cancel_token.is_cancelled() {
        // Init gRPC services
        services::init(
            &config,
            cla_registry,
            app_registry,
            ingress,
            dispatcher,
            &mut task_set,
            cancel_token.clone(),
        );
    }

    // Wait for all tasks to finish
    if !cancel_token.is_cancelled() {
        info!("Started successfully");
    }

    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly")
    }

    info!("Stopped");
}
