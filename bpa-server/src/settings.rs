use super::*;
use hardy_bpv7::prelude as bpv7;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

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

fn init_logger(config: &config::Config) {
    let log_level = get(config, "log_level")
        .expect("Invalid 'log_level' value in configuration")
        .unwrap_or("info")
        .parse::<tracing_subscriber::filter::LevelFilter>()
        .expect("Invalid log level");

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(
            log_level > tracing_subscriber::filter::LevelFilter::from_level(tracing::Level::INFO),
        )
        .init();
}

pub fn init() -> Option<(config::Config, bool)> {
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

    let mut b = config::Config::builder();

    // Add config file
    let config_source: String;
    if let Some(source) = flags.opt_str("config") {
        config_source =
            format!("Using base configuration file '{source}' specified on command line");
        b = b.add_source(config::File::with_name(&source).format(config::FileFormat::Toml))
    } else if let Ok(source) = std::env::var("HARDY_BPA_SERVER_CONFIG_FILE") {
        config_source = format!(
            "Using base configuration file '{source}' specified by HARDY_BPA_SERVER_CONFIG_FILE environment variable"
        );
        b = b.add_source(config::File::with_name(&source).format(config::FileFormat::Toml))
    } else {
        let path = config_dir().join(format!("{}.config", built_info::PKG_NAME));
        config_source = format!(
            "Using optional base configuration file '{}'",
            path.display()
        );
        b = b.add_source(
            config::File::from(path)
                .required(false)
                .format(config::FileFormat::Toml),
        )
    }

    // Pull in environment vars
    b = b.add_source(config::Environment::with_prefix("HARDY_BPA_SERVER"));

    // And parse...
    let config = b.build().expect("Failed to load configuration");

    init_logger(&config);
    info!(
        "{} version {} starting...",
        built_info::PKG_NAME,
        built_info::PKG_VERSION
    );
    info!("{config_source}");

    Some((config, flags.opt_present("u")))
}

pub fn get<'de, T: serde::Deserialize<'de>>(
    config: &config::Config,
    key: &str,
) -> Result<Option<T>, config::ConfigError> {
    match config.get::<T>(key) {
        Ok(v) => Ok(Some(v)),
        Err(config::ConfigError::NotFound(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

fn init_metadata_storage(
    config: &config::Config,
    upgrade: bool,
) -> Option<Arc<dyn hardy_bpa::storage::MetadataStorage>> {
    cfg_if::cfg_if! {
        if #[cfg(feature = "sqlite-storage")] {
            const DEFAULT: &str = hardy_sqlite_storage::CONFIG_KEY;
        } else {
            const DEFAULT: &str = "";
        }
    }

    let engine = get(config, "metadata_storage")
        .trace_expect("Invalid 'metadata_storage' value in configuration")
        .unwrap_or(DEFAULT);
    info!("Using '{engine}' metadata storage engine");

    let config = config.get_table(engine).unwrap_or_default();
    match engine {
        #[cfg(feature = "sqlite-storage")]
        hardy_sqlite_storage::CONFIG_KEY => {
            Some(hardy_sqlite_storage::Storage::init(&config, upgrade))
        }
        "" => None,
        _ => panic!("Unknown metadata storage engine: {engine}"),
    }
}

fn init_bundle_storage(
    config: &config::Config,
    _upgrade: bool,
) -> Option<Arc<dyn hardy_bpa::storage::BundleStorage>> {
    cfg_if::cfg_if! {
        if #[cfg(feature = "localdisk-storage")] {
            const DEFAULT: &str = hardy_localdisk_storage::CONFIG_KEY;
        } else {
            const DEFAULT: &str = "";
        }
    }

    let engine = get(config, "bundle_storage")
        .trace_expect("Invalid 'bundle_storage' value in configuration")
        .unwrap_or(DEFAULT);
    info!("Using '{engine}' bundle storage engine");

    let config = config.get_table(engine).unwrap_or_default();
    match engine {
        #[cfg(feature = "localdisk-storage")]
        hardy_localdisk_storage::CONFIG_KEY => {
            Some(hardy_localdisk_storage::Storage::init(&config))
        }
        "" => None,
        _ => panic!("Unknown bundle storage engine: {engine}"),
    }
}

fn load_admin_endpoints(config: &config::Config) -> Vec<bpv7::Eid> {
    match get::<config::Value>(config, "administrative_endpoints")
        .trace_expect("Invalid administrative_endpoints in configuration")
    {
        None => {
            info!(
                "No 'administrative_endpoints' values in configuration, falling back to random ipn NodeId"
            );
            Vec::new()
        }
        Some(v) => match v.kind {
            config::ValueKind::String(s) => {
                let eid = s
                    .parse()
                    .trace_expect("Invalid administrative endpoint '{s}' in configuration");
                match eid {
                    bpv7::Eid::LegacyIpn { .. } | bpv7::Eid::Ipn { .. } | bpv7::Eid::Dtn { .. } => {
                        vec![eid]
                    }
                    _ => {
                        error!("Invalid administrative endpoint '{eid}' in configuration");
                        panic!("Invalid administrative endpoint '{eid}' in configuration");
                    }
                }
            }
            config::ValueKind::Array(a) => {
                let mut eids = Vec::new();
                for v in &a {
                    let config::ValueKind::String(s) = &v.kind else {
                        warn!(
                            "Ignoring invalid administrative endpoint value '{v}' in configuration"
                        );
                        continue;
                    };
                    let Ok(eid) = s.parse() else {
                        warn!("Ignoring invalid administrative endpoint '{s}' in configuration");
                        continue;
                    };
                    match eid {
                        bpv7::Eid::LegacyIpn { .. }
                        | bpv7::Eid::Ipn { .. }
                        | bpv7::Eid::Dtn { .. } => {
                            eids.push(eid);
                        }
                        _ => {
                            error!("Invalid administrative endpoint '{eid}' in configuration");
                            panic!("Invalid administrative endpoint '{eid}' in configuration");
                        }
                    }
                }

                if eids.is_empty() {
                    if a.is_empty() {
                        info!(
                            "No 'administrative_endpoints' values in configuration, falling back to random ipn NodeId"
                        );
                    } else {
                        error!("No valid 'administrative_endpoints' values in configuration");
                        panic!("No valid 'administrative_endpoints' values in configuration");
                    }
                }
                eids
            }
            e => {
                error!("Invalid 'administrative_endpoints' value '{e}' in configuration");
                panic!("Invalid 'administrative_endpoints' value '{e}' in configuration");
            }
        },
    }
}

fn load_ipn_2_element(config: &config::Config) -> Option<bpv7::EidPatternMap<(), ()>> {
    get::<String>(config, "ipn_2_element")
        .trace_expect("Invalid 'ipn_2_element' value in configuration")
        .map(|v| {
            let mut m = bpv7::EidPatternMap::new();
            m.insert(
                &v.parse().trace_expect(&format!("Invalid EID pattern '{v}")),
                (),
                (),
            );
            m
        })
}

pub fn load_bpa_config(config: &config::Config, upgrade: bool) -> hardy_bpa::bpa::Config {
    let default_config = hardy_bpa::bpa::Config::default();
    let config = hardy_bpa::bpa::Config {
        status_reports: get(config, "status_reports")
            .trace_expect("Invalid 'status_reports' value in configuration")
            .unwrap_or(default_config.status_reports),
        wait_sample_interval: get(config, "wait_sample_interval")
            .trace_expect("Invalid 'wait_sample_interval' value in configuration")
            .map(|v: u64| time::Duration::seconds(v as i64))
            .unwrap_or(default_config.wait_sample_interval),

        max_forwarding_delay: get(config, "max_forwarding_delay")
            .trace_expect("Invalid 'max_forwarding_delay' value in configuration")
            .unwrap_or(default_config.max_forwarding_delay)
            .min(1u32),
        metadata_storage: init_metadata_storage(config, upgrade),
        bundle_storage: init_bundle_storage(config, upgrade),
        admin_endpoints: load_admin_endpoints(config),
        ipn_2_element: load_ipn_2_element(config),
    };

    if config.status_reports {
        info!("Bundle status reports are enabled");
    }

    if config.max_forwarding_delay == 0 {
        info!("Forwarding synchronization delay disabled by configuration");
    }

    config
}
