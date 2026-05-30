use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hardy_bpv7::eid::Eid;
use tracing::{debug, warn};

static COUNTER: AtomicU64 = AtomicU64::new(0);

pub async fn write_to_dir(dir: &Path, payload: &[u8], source: &Eid) {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);

    let filename = format!("{source}_{timestamp}_{seq}").replace(['\\', '/', ':', ' '], "_");
    let path = dir.join(filename);

    match tokio::fs::write(&path, payload).await {
        Ok(()) => {
            debug!(
                source = %source,
                bytes = payload.len(),
                "Wrote payload to '{}'",
                path.display()
            );
        }
        Err(e) => {
            warn!(
                source = %source,
                "Failed to write payload to '{}': {e}",
                path.display()
            );
        }
    }
}
