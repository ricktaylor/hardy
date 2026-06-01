use core::time::Duration;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hardy_async::sync::spin::Once;
use hardy_async::{TaskPool, async_trait};
use hardy_bpa::Bytes;
use hardy_bpa::services::{Application, ApplicationSink, StatusNotify};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use time::OffsetDateTime;
use tracing::{error, info, warn};

mod error;
mod inbox;
mod outbox;

pub use error::Error;

const DEFAULT_LIFETIME: Duration = Duration::from_secs(86400);

fn ensure_dir(path: &Path) -> Result<(), Error> {
    std::fs::create_dir_all(path).map_err(|e| Error::CreateDir {
        path: path.display().to_string(),
        source: e,
    })?;
    Ok(())
}

pub struct FileService {
    destination: Eid,
    lifetime: Duration,
    outbox: Option<PathBuf>,
    inbox: Option<PathBuf>,
    sink: Once<Arc<dyn ApplicationSink>>,
    tasks: TaskPool,
}

impl FileService {
    pub fn new(
        destination: Eid,
        lifetime: Option<Duration>,
        outbox: Option<PathBuf>,
        inbox: Option<PathBuf>,
    ) -> Result<Self, Error> {
        let lifetime = lifetime.unwrap_or(DEFAULT_LIFETIME);

        if let Some(path) = &outbox {
            ensure_dir(path)?;
        }
        if let Some(path) = &inbox {
            ensure_dir(path)?;
        }

        Ok(Self {
            destination,
            lifetime,
            outbox,
            inbox,
            sink: Once::new(),
            tasks: TaskPool::new(),
        })
    }

    pub async fn unregister(&self) {
        if let Some(sink) = self.sink.get() {
            sink.unregister().await;
        }
    }
}

#[async_trait]
impl Application for FileService {
    async fn on_register(&self, source: &Eid, sink: Box<dyn ApplicationSink>) {
        info!("File service registered at {source}");

        let sink: &Arc<dyn ApplicationSink> = self.sink.call_once(|| Arc::from(sink));

        if let Some(outbox) = &self.outbox {
            if let Err(e) = outbox::start(
                &self.tasks,
                sink.clone(),
                outbox.clone(),
                self.destination.clone(),
                self.lifetime,
            ) {
                error!("Failed to start outbox watcher: {e}");
            }
        }
    }

    async fn on_unregister(&self) {
        self.tasks.shutdown().await;
    }

    async fn on_receive(
        &self,
        source: Eid,
        _expiry: OffsetDateTime,
        _ack_requested: bool,
        payload: Bytes,
    ) {
        if let Some(inbox) = &self.inbox {
            inbox::write_to_dir(inbox, &payload, &source).await;
        } else {
            warn!("Received payload from {source} but no inbox configured");
        }
    }

    async fn on_status_notify(
        &self,
        _bundle_id: &Id,
        _from: &Eid,
        _kind: StatusNotify,
        _reason: ReasonCode,
        _timestamp: Option<OffsetDateTime>,
    ) {
    }
}
