/// Configuration for the SQLite metadata storage backend.
///
/// All fields have sensible defaults and can be overridden via TOML config
/// or environment variables (kebab-case field names).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct Config {
    /// Directory in which the database file is stored.
    ///
    /// Defaults to the platform-specific cache directory for the project
    /// (e.g. `~/.cache/sqlite-storage` on Linux), or `/var/spool/<pkg>` on
    /// Unix when no project directory can be determined.
    pub db_dir: std::path::PathBuf,
    /// Filename of the SQLite database. Defaults to `"metadata.db"`.
    pub db_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_dir:  directories::ProjectDirs::from("dtn", "Hardy", env!("CARGO_PKG_NAME"))
            .map_or_else(
                || {
                    #[cfg(unix)]
                    return std::path::Path::new("/var/spool").join(env!("CARGO_PKG_NAME"));

                    #[cfg(windows)]
                    return std::env::current_exe().expect("Failed to get current executable path").join(env!("CARGO_PKG_NAME"));

                    #[cfg(not(any(unix,windows)))]
                    compile_error!("No idea how to determine default sqlite metadata store directory for target platform");
                },
                |project_dirs| {
                    project_dirs.cache_dir().into()
                    // Lin: /home/alice/.store/barapp
                    // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\store
                    // Mac: /Users/Alice/Library/stores/com.Foo-Corp.Bar-App
                },
            ),
            db_name: String::from("metadata.db"),
        }
    }
}
