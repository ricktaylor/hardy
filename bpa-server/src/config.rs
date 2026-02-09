use super::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::path::PathBuf;
use std::str::FromStr;
use tracing::Level;

mod log_level_serde {
    use super::*;

    pub fn serialize<S>(level: &Option<Level>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match level {
            Some(level) => serializer.serialize_some(level.as_str()),
            None => serializer.serialize_none(),
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<Level>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s: Option<String> = Option::deserialize(deserializer)?;
        s.map(|s| Level::from_str(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum MetadataStorage {
    #[serde(rename = "memory")]
    Memory(Option<hardy_bpa::storage::metadata_mem::Config>),

    #[cfg(feature = "sqlite-storage")]
    #[serde(rename = "sqlite")]
    Sqlite(Option<hardy_sqlite_storage::Config>),
    // #[cfg(feature = "postgres-storage")]
    // #[serde(rename = "postgres")]
    // Postgres(PostgresConfig),
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum BundleStorage {
    #[serde(rename = "memory")]
    Memory(Option<hardy_bpa::storage::bundle_mem::Config>),

    #[cfg(feature = "localdisk-storage")]
    #[serde(rename = "localdisk")]
    LocalDisk(Option<hardy_localdisk_storage::Config>),
    // #[cfg(feature = "s3-storage")]
    // #[serde(rename = "s3")]
    // S3(S3Config),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Config {
    // Logging level
    #[serde(default, with = "log_level_serde")]
    pub log_level: Option<Level>,

    // Static Routes Configuration
    #[serde(default)]
    pub static_routes: Option<static_routes::Config>,

    // Flattened BPA settings
    #[serde(flatten, default)]
    pub bpa: hardy_bpa::config::Config,

    // gRPC options
    #[cfg(feature = "grpc")]
    #[serde(default)]
    pub grpc: Option<grpc::Config>,

    // Metadata Storage Configuration
    #[serde(default)]
    pub metadata_storage: Option<MetadataStorage>,

    // Bundle Storage Configuration
    #[serde(default)]
    pub bundle_storage: Option<BundleStorage>,

    #[cfg(feature = "ipn-legacy-filter")]
    #[serde(default, rename = "ipn-legacy-nodes")]
    pub ipn_legacy_nodes: hardy_ipn_legacy_filter::Config,

    // Convergence Layer Adaptors (CLAs)
    #[serde(default)]
    pub clas: Vec<clas::Cla>,

    #[serde(skip)]
    pub upgrade_storage: bool,

    #[serde(skip)]
    pub recover_storage: bool,
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
        .optflag(
            "r",
            "recover-store",
            "attempt to recover any damaged records in the store",
        )
        .optopt("c", "config", "use a custom configuration file", "FILE");
    opts
}

pub fn config_dir() -> PathBuf {
    directories::ProjectDirs::from("dtn", "Hardy", env!("CARGO_PKG_NAME")).map_or_else(
        || {
            #[cfg(all(target_os = "linux", not(feature = "packaged-installation")))]
            return std::path::Path::new("/etc/opt").join(env!("CARGO_PKG_NAME"));

            #[cfg(all(
                unix,
                not(all(target_os = "linux", not(feature = "packaged-installation")))
            ))]
            return std::path::Path::new("/etc").join(env!("CARGO_PKG_NAME"));

            #[cfg(windows)]
            return std::env::current_exe()
                .trace_expect("Failed to get current executable path")
                .join(env!("CARGO_PKG_NAME"));

            #[cfg(not(any(unix, windows)))]
            compile_error!("No idea how to determine default config directory for target platform");
        },
        |proj_dirs| {
            proj_dirs.config_local_dir().to_path_buf()
            // Lin: /home/alice/.config/barapp
            // Win: C:\Users\Alice\AppData\Roaming\Foo Corp\Bar App\config
            // Mac: /Users/Alice/Library/Application Support/com.Foo-Corp.Bar-App
        },
    )
}

pub fn init() -> Option<(Config, String)> {
    // Parse cmdline
    let opts = options();
    let args: Vec<String> = std::env::args().collect();
    let flags = opts
        .parse(&args[1..])
        .expect("Failed to parse command line args");
    if flags.opt_present("h") {
        let brief = format!(
            "{} {} - {}\n\nUsage: {} [options]",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_DESCRIPTION"),
            args[0]
        );
        print!("{}", opts.usage(&brief));
        return None;
    }
    if flags.opt_present("v") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return None;
    }

    let mut b = ::config::Config::builder();

    // Add config file
    let config_source: String;
    if let Some(source) = flags.opt_str("config") {
        config_source = format!("Using configuration file '{source}' specified on command line");
        b = b.add_source(::config::File::with_name(&source))
    } else if let Ok(source) = std::env::var("HARDY_BPA_SERVER_CONFIG_FILE") {
        config_source = format!(
            "Using configuration file '{source}' specified by HARDY_BPA_SERVER_CONFIG_FILE environment variable"
        );
        b = b.add_source(::config::File::with_name(&source))
    } else {
        let path = config_dir().join(format!("{}.yaml", env!("CARGO_PKG_NAME")));
        config_source = format!("Using configuration file '{}'", path.display());
        b = b.add_source(::config::File::from(path).required(false))
    }

    // Pull in environment vars
    b = b.add_source(::config::Environment::with_prefix("HARDY_BPA_SERVER"));

    let mut config: Config = b
        .build()
        .expect("Failed to read configuration")
        .try_deserialize()
        .expect("Failed to parse configuration");

    config.upgrade_storage = flags.opt_present("u");
    config.recover_storage = flags.opt_present("r");

    // And parse...
    Some((config, config_source))
}
