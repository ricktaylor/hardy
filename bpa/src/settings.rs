use super::*;
use std::path::PathBuf;

fn options() -> getopts::Options {
    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "print this help menu")
        .optflag("v", "version", "print the version information")
        .optopt("c", "config", "use a custom configuration file", "FILE");
    opts
}

fn defaults() -> Vec<(&'static str, config::Value)> {
    vec![
        ("grpc_address", "[::1]:50051".into()),
        ("log_level", "info".into()),
    ]
}

fn config_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME).map_or_else(
        || {
            log::warn!("Failed to resolve local config directory");
            None
        },
        |proj_dirs| {
            Some(proj_dirs.config_local_dir().to_path_buf())
            // Lin: /home/alice/.config/barapp
            // Win: C:\Users\Alice\AppData\Roaming\Foo Corp\Bar App\config
            // Mac: /Users/Alice/Library/Application Support/com.Foo-Corp.Bar-App
        },
    )
}

fn cache_dir() -> Option<config::Value> {
    directories::ProjectDirs::from("dtn", "Hardy", built_info::PKG_NAME).map_or_else(
        || {
            log::warn!("Failed to resolve local config directory");
            None
        },
        |proj_dirs| {
            proj_dirs.cache_dir().to_str().map(|p| p.into())
            // Lin: /home/alice/.cache/barapp
            // Win: C:\Users\Alice\AppData\Local\Foo Corp\Bar App\cache
            // Mac: /Users/Alice/Library/Caches/com.Foo-Corp.Bar-App
        },
    )
}

pub fn init() -> Option<config::Config> {
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

    // Set defaults
    let mut b = defaults()
        .iter()
        .fold(config::Config::builder(), |b, (k, v)| {
            b.set_default(k, v.clone())
                .expect("Invalid default config value")
        });

    // Add config file
    if let Some(source) = flags.opt_str("config") {
        b = b.add_source(config::File::with_name(&source));
    } else if let Ok(source) = std::env::var("HARDY_BPA_CONFIG_FILE") {
        b = b.add_source(config::File::with_name(&source));
    } else if let Some(path) = config_dir() {
        b = b.add_source(config::File::from(path).required(false));
    }

    // Add cache_dir default
    if let Some(cache_dir) = cache_dir() {
        b = b
            .set_default("cache_dir", cache_dir)
            .expect("Invalid default cache_dir config value");
    }

    // Pull in environment vars
    b = b.add_source(config::Environment::with_prefix("HARDY_BPA"));

    // And parse...
    Some(b.build().expect("Failed to load configuration"))
}
