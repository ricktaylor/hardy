use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hardy_bpv7::eid::Eid;
use tracing::{debug, error};

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
            error!(
                source = %source,
                "Failed to write payload to '{}': {e}",
                path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[tokio::test]
    async fn write_creates_file_with_correct_content() {
        let dir = tempfile::tempdir().unwrap();
        let source = Eid::from_str("ipn:1.42").unwrap();

        write_to_dir(dir.path(), b"hello", &source).await;

        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(entries.len(), 1);

        let content = std::fs::read(entries[0].path()).unwrap();
        assert_eq!(content, b"hello");
    }

    #[tokio::test]
    async fn write_generates_unique_filenames() {
        let dir = tempfile::tempdir().unwrap();
        let source = Eid::from_str("ipn:1.42").unwrap();

        write_to_dir(dir.path(), b"first", &source).await;
        write_to_dir(dir.path(), b"second", &source).await;

        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(entries.len(), 2);

        let names: Vec<_> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_ne!(names[0], names[1]);
    }

    #[tokio::test]
    async fn filename_sanitizes_special_characters() {
        let dir = tempfile::tempdir().unwrap();
        let source = Eid::from_str("dtn://node/svc").unwrap();

        write_to_dir(dir.path(), b"data", &source).await;

        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().flatten().collect();
        let name = entries[0].file_name().to_string_lossy().to_string();
        assert!(!name.contains('/'));
        assert!(!name.contains(':'));
    }
}
