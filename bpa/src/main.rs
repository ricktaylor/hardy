mod app_registry;
mod bundle;
mod cla_registry;
mod dispatcher;
mod fib;
mod ingress;
mod services;
mod store;
mod utils;

// This is the generic Error type used almost everywhere
type Error = Box<dyn std::error::Error + Send + Sync>;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

// This is the effective prelude
use trace_err::*;
use tracing::{error, info, instrument, trace, warn};

#[tokio::main]
async fn main() {
    // Parse command line
    let Some((config, upgrade, config_source)) = utils::settings::init() else {
        return;
    };

    // Init logger
    utils::logger::init(&config);
    info!("Version {} starting...", built_info::PKG_VERSION);
    info!("{config_source}");

    // Get administrative endpoints
    let administrative_endpoints = bundle::AdminEndpoints::init(&config);

    // New store
    let store = store::Store::new(&config, upgrade);

    // New registries
    let cla_registry = cla_registry::ClaRegistry::new(&config);
    let app_registry = app_registry::AppRegistry::new(&config, administrative_endpoints.clone());

    // New FIB
    let fib = fib::Fib::new(&config);

    // Prepare for graceful shutdown
    let (mut task_set, cancel_token) = utils::cancel::new_cancellable_set();

    // Create a new dispatcher
    let dispatcher = dispatcher::Dispatcher::new(
        &config,
        administrative_endpoints,
        store.clone(),
        cla_registry.clone(),
        app_registry.clone(),
        fib.clone(),
        &mut task_set,
        cancel_token.clone(),
    );

    // Create a new ingress
    let ingress = ingress::Ingress::new(
        &config,
        store.clone(),
        dispatcher.clone(),
        &mut task_set,
        cancel_token.clone(),
    );

    // Init gRPC services
    services::init(
        &config,
        cla_registry,
        app_registry,
        ingress.clone(),
        dispatcher.clone(),
        &mut task_set,
        cancel_token.clone(),
    );

    // Restart the store - this can take a while as the store is walked
    store
        .restart(ingress, dispatcher, cancel_token.clone())
        .await;

    // Wait for all tasks to finish
    if !cancel_token.is_cancelled() {
        info!("Started successfully");
    }
    while let Some(r) = task_set.join_next().await {
        r.trace_expect("Task terminated unexpectedly")
    }

    info!("Stopped");
}
