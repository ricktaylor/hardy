use core::time::Duration;
use std::path::PathBuf;
use std::sync::Arc;

use hardy_async::{CancellationToken, TaskPool};
use hardy_bpa::services::ApplicationSink;
use hardy_bpv7::eid::Eid;
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

pub fn start(
    tasks: &TaskPool,
    sink: Arc<dyn ApplicationSink>,
    send_path: PathBuf,
    destination: Eid,
    lifetime: Duration,
) {
    let cancel_token = tasks.cancel_token().clone();
    hardy_async::spawn!(tasks, "fifo_reader", async move {
        run(sink, send_path, destination, lifetime, cancel_token).await;
    });
}

async fn run(
    sink: Arc<dyn ApplicationSink>,
    path: PathBuf,
    destination: Eid,
    lifetime: Duration,
    cancel: CancellationToken,
) {
    info!("Reading from FIFO '{}'", path.display());

    loop {
        tokio::select! {
            result = read_one_payload(&path) => {
                match result {
                    Ok(payload) if payload.is_empty() => continue,
                    Ok(payload) => {
                        debug!(dest = %destination, bytes = payload.len(), "Sending payload from FIFO");
                        match sink.send(destination.clone(), payload.into(), lifetime, None).await {
                            Ok(id) => debug!("Sent bundle {id}"),
                            Err(e) => warn!("Failed to send bundle: {e}"),
                        }
                    }
                    Err(e) => {
                        error!("Failed to read from FIFO '{}': {e}", path.display());
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
            _ = cancel.cancelled() => break,
        }
    }

    info!("Stopped reading from FIFO '{}'", path.display());
}

// Each call reopens the FIFO: a reader fd reaches EOF when the writer closes,
// so we must reopen to accept the next writer.
async fn read_one_payload(path: &PathBuf) -> std::io::Result<Vec<u8>> {
    let mut file = tokio::net::unix::pipe::OpenOptions::new().open_receiver(path)?;
    file.readable().await?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).await?;
    Ok(buf)
}
