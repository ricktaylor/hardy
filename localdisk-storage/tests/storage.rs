use std::path::{Path, PathBuf};

use hardy_bpa::{Bytes, storage::BundleStorage};
use storage_tests::VecSink;

use hardy_localdisk_storage::LocalDiskStorage;

// LD-01: Files are created under the configured store_dir.
#[tokio::test]
async fn test_configuration_custom_store_dir() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

    let name = store.save(Bytes::from_static(b"hello")).await.unwrap();
    let full_path = dir.path().join(&*name);
    assert!(
        full_path.exists(),
        "bundle file should exist under configured store_dir"
    );
}

// LD-02: Recovery cleans up .tmp files, zero-byte placeholders, and empty dirs.
#[tokio::test]
async fn test_recovery_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

    // Save a valid bundle
    let valid_name = store.save(Bytes::from_static(b"valid")).await.unwrap();

    // Create a .tmp file (interrupted save)
    let tmp_dir = dir.path().join("aa").join("bb");
    std::fs::create_dir_all(&tmp_dir).unwrap();
    let tmp_path = tmp_dir.join("deadbeef.tmp");
    std::fs::write(&tmp_path, b"partial").unwrap();

    // Create a zero-byte placeholder (interrupted save)
    let placeholder_path = tmp_dir.join("00000000");
    std::fs::File::create(&placeholder_path).unwrap();

    // Create an empty subdirectory
    let empty_dir = dir.path().join("cc").join("dd");
    std::fs::create_dir_all(&empty_dir).unwrap();

    // Run recovery
    let sink = VecSink::new();
    store.recover(&sink).await.unwrap();

    // Collect recovered entries
    let recovered = sink.into_inner();

    // Only the valid bundle should be recovered
    assert_eq!(recovered.len(), 1, "should recover exactly 1 valid bundle");
    assert_eq!(&*recovered[0].0, &*valid_name);

    // .tmp file should be cleaned up
    assert!(!tmp_path.exists(), ".tmp file should be removed");

    // Zero-byte placeholder should be cleaned up
    assert!(
        !placeholder_path.exists(),
        "zero-byte placeholder should be removed"
    );

    // Empty directory should be cleaned up (inner `dd` removed, `cc` may also be removed)
    assert!(!empty_dir.exists(), "empty directory should be removed");
}

// LD-03: Files are distributed in a two-level xx/yy/ directory structure.
#[tokio::test]
async fn test_filesystem_structure() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

    // Save several bundles
    let mut names = Vec::new();
    for _ in 0..5 {
        names.push(store.save(Bytes::from_static(b"data")).await.unwrap());
    }

    for name in &names {
        let parts: Vec<&str> = name.split(std::path::MAIN_SEPARATOR).collect();
        assert_eq!(
            parts.len(),
            3,
            "storage name '{name}' should have 3 path components (xx/yy/file)"
        );
        // First two components should be 2-char hex directories
        assert_eq!(
            parts[0].len(),
            2,
            "first dir component should be 2 hex chars"
        );
        assert_eq!(
            parts[1].len(),
            2,
            "second dir component should be 2 hex chars"
        );
        assert!(
            u8::from_str_radix(parts[0], 16).is_ok(),
            "first dir component '{}' should be valid hex",
            parts[0]
        );
        assert!(
            u8::from_str_radix(parts[1], 16).is_ok(),
            "second dir component '{}' should be valid hex",
            parts[1]
        );
    }
}

// LD-04: With fsync=true, save writes to .tmp then renames (no .tmp left behind).
#[tokio::test]
async fn test_atomic_save_no_tmp_residue() {
    let dir = tempfile::tempdir().unwrap();
    let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(true), false);

    let name = store.save(Bytes::from_static(b"atomic")).await.unwrap();

    // The final file should exist
    let full_path = dir.path().join(&*name);
    assert!(full_path.exists(), "final file should exist");

    // No .tmp files should remain anywhere
    let has_tmp = walkdir(dir.path())
        .iter()
        .any(|p| p.extension().is_some_and(|e| e == "tmp"));
    assert!(!has_tmp, "no .tmp files should remain after save");

    // Data should be correct
    let loaded = store.load(&name).await.unwrap().unwrap();
    assert_eq!(&*loaded, b"atomic");
}

// LD-05: Save on a read-only directory returns an error, not a panic.
//
// Unix-only: the Windows read-only attribute on a directory does not
// block file creation inside it, so the scenario cannot be built there.
#[cfg(unix)]
#[tokio::test]
async fn test_save_to_readonly_dir_returns_error() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let store = LocalDiskStorage::new(Some(dir.path().to_path_buf()), Some(false), false);

    // Make the store directory read-only
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

    // Probe: a privileged process (e.g. root, common in CI containers)
    // bypasses permission bits, so save() cannot be made to fail here.
    if std::fs::write(dir.path().join("probe"), b"probe").is_ok() {
        eprintln!(
            "skipping test_save_to_readonly_dir_returns_error assertion: the read-only directory does not block writes (privileged process?)"
        );
    } else {
        // save() should return an error, not panic
        let result = store.save(Bytes::from_static(b"fail")).await;
        assert!(result.is_err(), "save to read-only dir should return Err");
    }

    // Restore permissions so tempdir cleanup succeeds
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
}

// Recursively collect all file paths under a directory.
fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                files.extend(walkdir(&path));
            } else {
                files.push(path);
            }
        }
    }
    files
}
