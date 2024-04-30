use super::*;
use std::path::{Path, PathBuf};

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

fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME).map_or_else(
        || {
            if cfg!(all(
                target_os = "linux",
                not(feature = "packaged-installation")
            )) {
                Some(Path::new("/etc/opt").join(built_info::PKG_NAME))
            } else if cfg!(unix) {
                Some(Path::new("/etc").join(built_info::PKG_NAME))
            } else {
                log::warn!("Failed to determine default configuration directory");
                None
            }
        },
        |proj_dirs| {
            Some(proj_dirs.config_local_dir().to_path_buf())
            // Lin: /home/alice/.config/barapp
            // Win: C:\Users\Alice\AppData\Roaming\Foo Corp\Bar App\config
            // Mac: /Users/Alice/Library/Application Support/com.Foo-Corp.Bar-App
        },
    )
}

pub fn get_with_default<'de, T: serde::Deserialize<'de>, D: Into<T>>(
    config: &config::Config,
    key: &str,
    default: D,
) -> Result<T, anyhow::Error> {
    match config.get::<T>(key) {
        Ok(v) => Ok(v),
        Err(config::ConfigError::NotFound(_)) => Ok(default.into()),
        Err(e) => Err(anyhow!("Failed to parse config value '{}': {}", key, e)),
    }
}

pub fn init() -> Option<(config::Config, bool, String)> {
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
        config_source = format!(
            "Using base configuration file '{}' specified on command line",
            &source
        );
        b = b.add_source(config::File::with_name(&source).format(config::FileFormat::Toml))
    } else if let Ok(source) = std::env::var("HARDY_BPA_CONFIG_FILE") {
        config_source = format!("Using base configuration file '{}' specified by HARDY_BPA_CONFIG_FILE environment variable",&source);
        b = b.add_source(config::File::with_name(&source).format(config::FileFormat::Toml))
    } else if let Some(path) = config_dir() {
        config_source = format!(
            "Using optional base configuration file '{}'",
            path.to_string_lossy()
        );
        b = b.add_source(
            config::File::from(path)
                .required(false)
                .format(config::FileFormat::Toml),
        )
    } else {
        config_source =
            "No base configuration file specified, and no suitable default found".to_string();
    }

    // Pull in environment vars
    b = b.add_source(config::Environment::with_prefix("HARDY_BPA"));

    // And parse...
    Some((
        b.build().expect("Failed to build configuration"),
        flags.opt_present("u"),
        config_source,
    ))
}
