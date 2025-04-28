use super::*;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub bpa: hardy_bpa::config::Config,
    pub static_routes: Option<static_routes::Config>,
}

mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

fn options() -> getopts::Options {
    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "print this help menu")
        .optflag("v", "version", "print the version information")
        .optflag(
            "u",
            "upgrade-store",
            "upgrade the bundle store to the current format",
        )
        .optopt("c", "config", "use a custom configuration file", "FILE");
    opts
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME).map_or_else(
        || {
            cfg_if::cfg_if! {
                if #[cfg(all(
                    target_os = "linux",
                    not(feature = "packaged-installation")
                ))] {
                    Path::new("/etc/opt").join(built_info::PKG_NAME)
                } else if #[cfg(unix)] {
                    Path::new("/etc").join(built_info::PKG_NAME)
                } else if #[cfg(windows)] {
                    std::env::current_exe().join(built_info::PKG_NAME)
                } else {
                    compile_error!("No idea how to determine default config directory for target platform")
                }
            }
        },
        |proj_dirs| {
            proj_dirs.config_local_dir().to_path_buf()
            // Lin: /home/alice/.config/barapp
            // Win: C:\Users\Alice\AppData\Roaming\Foo Corp\Bar App\config
            // Mac: /Users/Alice/Library/Application Support/com.Foo-Corp.Bar-App
        },
    )
}

pub fn init() -> Option<Config> {
    // Parse cmdline
    let opts = options();
    let args: Vec<String> = std::env::args().collect();
    let program = args[0].clone();
    let flags = opts
        .parse(&args[1..])
        .expect("Failed to parse command line args");
    if flags.opt_present("h") {
        let brief = format!(
            "{} {} - {}\n\nUsage: {} [options]",
            built_info::PKG_NAME,
            built_info::PKG_VERSION,
            built_info::PKG_DESCRIPTION,
            program
        );
        print!("{}", opts.usage(&brief));
        return None;
    }
    if flags.opt_present("v") {
        println!("{}", built_info::PKG_VERSION);
        return None;
    }

    let mut b = ::config::Config::builder();

    // Add config file
    let config_source: String;
    if let Some(source) = flags.opt_str("config") {
        config_source =
            format!("Using base configuration file '{source}' specified on command line");
        b = b.add_source(::config::File::with_name(&source))
    } else if let Ok(source) = std::env::var("HARDY_BPA_SERVER_CONFIG_FILE") {
        config_source = format!(
            "Using base configuration file '{source}' specified by HARDY_BPA_SERVER_CONFIG_FILE environment variable"
        );
        b = b.add_source(::config::File::with_name(&source))
    } else {
        let path = config_dir().join(format!("{}.toml", built_info::PKG_NAME));
        config_source = format!(
            "Using optional base configuration file '{}'",
            path.display()
        );
        b = b.add_source(
            ::config::File::from(path)
                .required(false)
                .format(::config::FileFormat::Toml),
        )
    }

    // Pull in environment vars
    b = b.add_source(::config::Environment::with_prefix("HARDY_BPA_SERVER"));

    // And parse...
    let config_table = b.build().expect("Failed to read configuration");

    let log_level = get(&config_table, "log_level")
        .expect("Invalid 'log_level' value in configuration")
        .unwrap_or("info".to_string())
        .parse::<tracing_subscriber::filter::LevelFilter>()
        .expect("Invalid 'log_level' value in configuration");

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(
            log_level > tracing_subscriber::filter::LevelFilter::from_level(tracing::Level::INFO),
        )
        .init();

    info!(
        "{} version {} starting...",
        built_info::PKG_NAME,
        built_info::PKG_VERSION
    );
    info!("{config_source}");

    let upgrade = flags.opt_present("u");

    let metadata_storage = init_metadata_storage(&config_table, upgrade);
    let bundle_storage = init_bundle_storage(&config_table, upgrade);

    let mut config: Config = config_table
        .try_deserialize()
        .expect("Failed to parse configuration");

    config.bpa.metadata_storage = metadata_storage;
    config.bpa.bundle_storage = bundle_storage;

    if config.bpa.status_reports {
        info!("Bundle status reports are enabled");
    }

    Some(config)
}

fn get<'de, T: serde::Deserialize<'de>>(
    config: &::config::Config,
    key: &str,
) -> Result<Option<T>, ::config::ConfigError> {
    match config.get::<T>(key) {
        Ok(v) => Ok(Some(v)),
        Err(::config::ConfigError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn init_metadata_storage(
    config: &::config::Config,
    upgrade: bool,
) -> Option<Arc<dyn hardy_bpa::storage::MetadataStorage>> {
    cfg_if::cfg_if! {
        if #[cfg(feature = "sqlite-storage")] {
            const DEFAULT: &str = hardy_sqlite_storage::Config::KEY;
        } else {
            const DEFAULT: &str = "";
        }
    }

    let engine = get(config, "metadata_storage")
        .trace_expect("Invalid 'metadata_storage' value in configuration")
        .unwrap_or(DEFAULT);
    info!("Using '{engine}' metadata storage engine");

    match engine {
        #[cfg(feature = "sqlite-storage")]
        hardy_sqlite_storage::Config::KEY => Some(hardy_sqlite_storage::Storage::init(
            config.get(engine).unwrap_or_default(),
            upgrade,
        )),
        "" => None,
        _ => panic!("Unknown metadata storage engine: {engine}"),
    }
}

fn init_bundle_storage(
    config: &::config::Config,
    upgrade: bool,
) -> Option<Arc<dyn hardy_bpa::storage::BundleStorage>> {
    cfg_if::cfg_if! {
        if #[cfg(feature = "localdisk-storage")] {
            const DEFAULT: &str = hardy_localdisk_storage::Config::KEY;
        } else {
            const DEFAULT: &str = "";
        }
    }

    let engine = get(config, "bundle_storage")
        .trace_expect("Invalid 'bundle_storage' value in configuration")
        .unwrap_or(DEFAULT);
    info!("Using '{engine}' bundle storage engine");

    match engine {
        #[cfg(feature = "localdisk-storage")]
        hardy_localdisk_storage::Config::KEY => Some(hardy_localdisk_storage::Storage::init(
            config.get(engine).unwrap_or_default(),
            upgrade,
        )),
        "" => None,
        _ => panic!("Unknown bundle storage engine: {engine}"),
    }
}
