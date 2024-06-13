use super::*;
use std::path::{Path, PathBuf};

fn options() -> getopts::Options {
    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "print this help menu")
        .optflag("v", "version", "print the version information")
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

pub fn get_with_default<'de, T: serde::Deserialize<'de>, D: Into<T>>(
    config: &config::Config,
    key: &str,
    default: D,
) -> Result<T, config::ConfigError> {
    match config.get::<T>(key) {
        Err(config::ConfigError::NotFound(_)) => Ok(default.into()),
        r => r,
    }
}

pub fn init() -> Option<(config::Config, String)> {
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
    } else if let Ok(source) = std::env::var("HARDY_TCPCL_CONFIG_FILE") {
        config_source = format!("Using base configuration file '{source}' specified by HARDY_TCPCL_CONFIG_FILE environment variable");
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
    b = b.add_source(config::Environment::with_prefix("HARDY_TCPCL"));

    // And parse...
    Some((
        b.build().expect("Failed to build configuration"),
        config_source,
    ))
}
