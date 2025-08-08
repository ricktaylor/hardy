use serde::{Deserialize, Serialize};

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(default)]
pub struct Config {
    pub db_dir: std::path::PathBuf,
    pub db_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            db_dir:  directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME)
            .map_or_else(
                || {
                    cfg_if::cfg_if! {
                        if #[cfg(unix)] {
                            std::path::Path::new("/var/spool").join(built_info::PKG_NAME)
                        } else if #[cfg(windows)] {
                            std::env::current_exe().join(built_info::PKG_NAME)
                        } else {
                            compile_error!("No idea how to determine default sqlite metadata store directory for target platform")
                        }
                    }
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
