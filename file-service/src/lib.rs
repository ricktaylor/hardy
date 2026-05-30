use core::time::Duration;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hardy_async::sync::spin::Once;
use hardy_async::{TaskPool, async_trait};
use hardy_bpa::Bytes;
use hardy_bpa::services::{Application, ApplicationSink, StatusNotify};
use hardy_bpv7::bundle::Id;
use hardy_bpv7::eid::Eid;
use hardy_bpv7::status_report::ReasonCode;
use nix::sys::stat::Mode;
use time::OffsetDateTime;
use tracing::{info, warn};

mod reader;
mod writer;

const DEFAULT_LIFETIME: Duration = Duration::from_secs(86400);

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Failed to create FIFO '{path}': {source}")]
    CreateFifo {
        path: String,
        source: std::io::Error,
    },

    #[error("Failed to create directory '{path}': {source}")]
    CreateDir {
        path: String,
        source: std::io::Error,
    },
}

fn ensure_fifo(path: &Path) -> Result<(), Error> {
    if path.exists() {
        let meta = std::fs::metadata(path).map_err(|e| Error::CreateFifo {
            path: path.display().to_string(),
            source: e,
        })?;
        if !meta.file_type().is_fifo() {
            return Err(Error::CreateFifo {
                path: path.display().to_string(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "path exists but is not a FIFO",
                ),
            });
        }
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| Error::CreateFifo {
                path: parent.display().to_string(),
                source: e,
            })?;
        }
    }

    nix::unistd::mkfifo(path, Mode::from_bits_truncate(0o660)).map_err(|e| Error::CreateFifo {
        path: path.display().to_string(),
        source: e.into(),
    })?;

    info!("Created FIFO at '{}'", path.display());
    Ok(())
}

fn ensure_dir(path: &Path) -> Result<(), Error> {
    if !path.exists() {
        std::fs::create_dir_all(path).map_err(|e| Error::CreateDir {
            path: path.display().to_string(),
            source: e,
        })?;
        info!("Created directory at '{}'", path.display());
    }
    Ok(())
}

pub struct FileService {
    destination: Eid,
    lifetime: Duration,
    send_path: Option<PathBuf>,
    recv_dir: Option<PathBuf>,
    sink: Once<Arc<dyn ApplicationSink>>,
    tasks: TaskPool,
}

impl FileService {
    pub fn new(
        destination: Eid,
        lifetime: Option<Duration>,
        send_path: Option<PathBuf>,
        recv_dir: Option<PathBuf>,
    ) -> Result<Self, Error> {
        let lifetime = lifetime.unwrap_or(DEFAULT_LIFETIME);

        if let Some(path) = &send_path {
            ensure_fifo(path)?;
        }
        if let Some(path) = &recv_dir {
            ensure_dir(path)?;
        }

        Ok(Self {
            destination,
            lifetime,
            send_path,
            recv_dir,
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

        if let Some(send_path) = &self.send_path {
            reader::start(
                &self.tasks,
                sink.clone(),
                send_path.clone(),
                self.destination.clone(),
                self.lifetime,
            );
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
        if let Some(dir) = &self.recv_dir {
            writer::write_to_dir(dir, &payload, &source).await;
        } else {
            warn!("Received payload from {source} but no recv_dir configured");
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
