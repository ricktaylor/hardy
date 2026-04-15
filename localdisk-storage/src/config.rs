/// Configuration for the local-disk bundle storage backend.
///
/// All fields have sensible defaults via the [`Default`] implementation.
/// When the `serde` feature is enabled, fields use kebab-case (e.g. `store-dir`).
#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default, rename_all = "kebab-case"))]
pub struct Config {
    /// Directory where bundle files are stored.
    ///
    /// Defaults to a platform-specific cache directory resolved via the `directories` crate
    /// (e.g. `~/.cache/hardy-localdisk-storage` on Linux), falling back to
    /// `/var/spool/hardy-localdisk-storage` on Unix or the executable directory on Windows.
    pub store_dir: std::path::PathBuf,

    /// Whether to use fsync for crash-safe atomic writes.
    ///
    /// When `true` (the default), each save writes to a `.tmp` file with `O_SYNC` /
    /// `FILE_FLAG_WRITE_THROUGH`, syncs data, renames to the final name, and syncs the
    /// parent directory. When `false`, plain `tokio::fs::write` is used instead.
    pub fsync: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            store_dir: directories::ProjectDirs::from("dtn", "Hardy", env!("CARGO_PKG_NAME")).map_or_else(
                || {
                    #[cfg(unix)]
                    return std::path::Path::new("/var/spool").join(env!("CARGO_PKG_NAME"));

                    #[cfg(windows)]
                    return std::env::current_exe().expect("Failed to get current exe").join(env!("CARGO_PKG_NAME"));

                    #[cfg(not(any(unix,windows)))]
                    compile_error!("No idea how to determine default localdisk bundle store directory for target platform");
                },
                |project_dirs| {
                    project_dirs.cache_dir().into()
                    // Lin: /home/alice/.cache/barapp
                    // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\cache
                    // Mac: /Users/Alice/Library/Caches/com.Foo-Corp.Bar-App
                },
            ),
            fsync: true
        }
    }
}
