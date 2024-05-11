use anyhow::anyhow;
use log_err::*;
use tracing::instrument;

mod app_registry;
mod bundle;
mod cla_registry;
mod dispatcher;
mod fib;
mod ingress;
mod logger;
mod node_id;
mod services;
mod settings;
mod store;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn listen_for_cancel(
    task_set: &mut tokio::task::JoinSet<()>,
    cancel_token: tokio_util::sync::CancellationToken,
) {
    cfg_if::cfg_if! {
        if #[cfg(unix)] {
            let mut term_handler =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .log_expect("Failed to register signal handlers");
        } else {
            let mut term_handler = std::future::pending();
        }
    }
    task_set.spawn(async move {
        tokio::select! {
            _ = term_handler.recv() =>
                {
                    // Signal stop
                    log::info!("Received terminate signal, stopping...");
                    cancel_token.cancel();
                }
            _ = tokio::signal::ctrl_c() =>
                {
                    // Signal stop
                    log::info!("Received CTRL+C, stopping...");
                    cancel_token.cancel();
                }
            _ = cancel_token.cancelled() => {}
        }
    });
}

async fn cancellable_sleep(
    duration: time::Duration,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> bool {
    if !duration.is_positive() {
        return true;
    }

    let timer = tokio::time::sleep(tokio::time::Duration::new(
        duration.whole_seconds() as u64,
        duration.subsec_nanoseconds() as u32,
    ));
    tokio::pin!(timer);

    tokio::select! {
        () = &mut timer => true,
        _ = cancel_token.cancelled() => false
    }
}

#[tokio::main]
async fn main() {
    // Parse command line
    let Some((config, upgrade, config_source)) = settings::init() else {
        return;
    };

    // Init logger
    logger::init(&config);
    log::info!("Version {} starting...", built_info::PKG_VERSION);
    log::info!("{}", config_source);

    // Get administrative_endpoint
    let administrative_endpoint =
        node_id::NodeId::init(&config).log_expect("Failed to load configuration");

    // New store
    let store = store::Store::new(&config, upgrade);

    // New registries
    let cla_registry = cla_registry::ClaRegistry::new(&config);
    let app_registry = app_registry::AppRegistry::new(&config, administrative_endpoint.clone());

    // New FIB
    let fib = fib::Fib::new(&config);

    // Prepare for graceful shutdown
    let cancel_token = tokio_util::sync::CancellationToken::new();
    let mut task_set = tokio::task::JoinSet::new();
    listen_for_cancel(&mut task_set, cancel_token.clone());

    // Create a new dispatcher
    let dispatcher = dispatcher::Dispatcher::new(
        &config,
        administrative_endpoint,
        store.clone(),
        cla_registry.clone(),
        app_registry.clone(),
        fib.clone(),
        &mut task_set,
        cancel_token.clone(),
    )
    .log_expect("Failed to initialize dispatcher");

    // Create a new ingress
    let ingress = ingress::Ingress::new(
        &config,
        store.clone(),
        dispatcher.clone(),
        fib,
        &mut task_set,
        cancel_token.clone(),
    )
    .log_expect("Failed to initialize ingress");

    // Init gRPC services
    services::init(
        &config,
        cla_registry,
        app_registry,
        ingress.clone(),
        dispatcher.clone(),
        &mut task_set,
        cancel_token.clone(),
    )
    .log_expect("Failed to start gRPC services");

    // Restart the store - this can take a while as the store is walked
    store
        .restart(ingress, dispatcher, cancel_token.clone())
        .await
        .log_expect("Store restart failed");

    // Wait for all tasks to finish
    if !cancel_token.is_cancelled() {
        log::info!("Started successfully");
    }
    while let Some(r) = task_set.join_next().await {
        r.log_expect("Task terminated unexpectedly")
    }

    log::info!("Stopped");
}
