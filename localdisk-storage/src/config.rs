#[derive(Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Config {
    pub store_dir: std::path::PathBuf,
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
