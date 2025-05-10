use super::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "config")]
pub enum MetadataStorage {
    #[cfg(feature = "sqlite-storage")]
    #[serde(rename = "sqlite")]
    Sqlite(hardy_sqlite_storage::Config),

    #[cfg(feature = "postgres-storage")]
    #[serde(rename = "postgres")]
    Postgres(PostgresConfig),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", content = "config")]
pub enum BundleStorage {
    #[cfg(feature = "localdisk-storage")]
    #[serde(rename = "localdisk")]
    LocalDisk(hardy_localdisk_storage::Config),

    #[cfg(feature = "s3-storage")]
    #[serde(rename = "s3")]
    S3(S3Config),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Cla {
    pub name: String,
    #[serde(flatten)]
    pub cla: ClaConfig,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "config")]
pub enum ClaConfig {
    TcpClv4(TcpclConfig),
    //UdpCl(UdpclConfig),
    //Btpu-Ethernet(BtpuEthernetConfig),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TcpclConfig {}

#[derive(Serialize, Deserialize, Debug, Default)]
pub struct Config {
    // Logging level
    pub log_level: String,

    // Static Routes Configuration
    pub static_routes: Option<static_routes::Config>,

    // Flattened BPA settings
    #[serde(flatten, default)]
    pub bpa: hardy_bpa::config::Config,

    // Metadata Storage Configuration
    #[serde(default)]
    pub metadata_storage: Option<MetadataStorage>,

    // Bundle Storage Configuration
    #[serde(default)]
    pub bundle_storage: Option<BundleStorage>,

    // Convergence Layer Adaptors (CLAs)
    #[serde(default)]
    pub clas: Vec<Cla>,
}

impl Config {
    fn set_bpa_storage(&mut self, upgrade: bool) {
        if let Some(config) = self.metadata_storage.take() {
            self.bpa.metadata_storage = match config {
                #[cfg(feature = "sqlite-storage")]
                MetadataStorage::Sqlite(config) => {
                    Some(hardy_sqlite_storage::Storage::init(config, upgrade))
                }

                #[cfg(feature = "postgres-storage")]
                MetadataStorage::Postgres(config) => todo!(),
            };
        }

        if let Some(config) = self.bundle_storage.take() {
            self.bpa.bundle_storage = match config {
                #[cfg(feature = "localdisk-storage")]
                BundleStorage::LocalDisk(config) => {
                    Some(hardy_localdisk_storage::Storage::init(config, upgrade))
                }

                #[cfg(feature = "s3-storage")]
                BundleStorage::S3(config) => todo!(),
            };
        }
    }
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
        let path = config_dir().join(format!("{}.yaml", built_info::PKG_NAME));
        config_source = format!(
            "Using optional base configuration file '{}'",
            path.display()
        );
        b = b.add_source(::config::File::from(path).required(false))
    }

    // Pull in environment vars
    b = b.add_source(::config::Environment::with_prefix("HARDY_BPA_SERVER"));

    // And parse...
    let mut config: Config = b
        .build()
        .expect("Failed to read configuration")
        .try_deserialize()
        .expect("Failed to parse configuration");

    // Start the logging
    let log_level = config
        .log_level
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

    // Initialize storage drivers
    config.set_bpa_storage(flags.opt_present("u"));

    Some(config)
}
