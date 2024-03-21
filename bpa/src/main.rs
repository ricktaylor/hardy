use log_err::*;
use tokio::signal::unix::{signal, SignalKind};
use tokio_util::sync::CancellationToken;

mod bundle;
mod cache;
mod cbor;
mod cla;
mod database;
mod logger;
mod services;
mod settings;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn listen_for_cancel(task_set: &mut tokio::task::JoinSet<()>, cancel_token: CancellationToken) {
    let mut term_handler =
        signal(SignalKind::terminate()).log_expect("Failed to register signal handlers");

    task_set.spawn(async move {
        tokio::select! {
            Some(_) = term_handler.recv() =>
                {
                    // Signal stop
                    log::info!("{} stopping...", built_info::PKG_NAME);
                    cancel_token.cancel();
                }
            _ = cancel_token.cancelled() => {}
        }
    });
}

#[tokio::main]
async fn main() {
    // load config
    let Some(config) = settings::init() else {
        return;
    };

    // Init logger
    logger::init(&config);
    log::info!("{} starting...", built_info::PKG_NAME);

    // Init DB
    let db = database::init(&config);

    // Prep graceful shutdown
    let cancel_token = CancellationToken::new();
    let mut task_set = tokio::task::JoinSet::new();
    listen_for_cancel(&mut task_set, cancel_token.clone());

    // Init bundle cache - this can take a while
    let Some(cache) = cache::init(&config, db, cancel_token.clone()).await else {
        return;
    };

    // Init async systems
    services::init(&config, cache, &mut task_set, cancel_token);

    log::info!("{} started", built_info::PKG_NAME);

    // Wait for all tasks to finish
    while let Some(r) = task_set.join_next().await {
        r.log_expect("Task terminated unexpectedly")
    }

    log::info!("{} stopped", built_info::PKG_NAME);
}
