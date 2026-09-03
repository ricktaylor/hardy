// Shared private-key file hygiene, used by both the BPSec key store and
// the gRPC TLS key so neither owns the check by coincidence.

use std::path::Path;

// Warns if `path` is readable by group or other. Unix-only; a no-op on
// platforms without POSIX permission bits.
#[cfg(unix)]
pub(crate) fn check_permissions(path: &Path) {
    use std::{fs, os::unix::fs::MetadataExt};

    use tracing::warn;

    if let Ok(meta) = fs::metadata(path) {
        let mode = meta.mode() & 0o777;
        if mode & 0o077 != 0 {
            warn!(
                "Key file '{}' has group/other permissions (mode {:04o}). \
                 Restrict to owner-only (chmod 0600).",
                path.display(),
                mode
            );
        }
    }
}

#[cfg(not(unix))]
pub(crate) fn check_permissions(_path: &Path) {}
