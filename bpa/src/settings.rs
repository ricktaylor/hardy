use lazy_static::lazy_static;
use serde::Deserialize;
use std::sync::OnceLock;

// Buildtime info
// Use of a mod or pub mod is not actually necessary.
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

// Config defaults
lazy_static! {
    static ref DEFAULTS: Vec<(&'static str, config::Value)> = vec! {
        ("grpc_addr","[::1]".into()),
        ("grpc_port",50051.into())
    };
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub grpc_addr: String,
    pub grpc_port: u16,
}

static INSTANCE: OnceLock<Config> = OnceLock::new();

impl Config {
    pub fn get() -> &'static Config {
        INSTANCE.get().expect("Configuration not loaded")
    }
}

pub fn init() -> Option<()> {
    Some(
        INSTANCE
            .set(load()?)
            .expect("Configuration loaded more than once"),
    )
}

fn load() -> Option<Config> {
    // Build command options
    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "print this help menu");
    opts.optflag("v", "version", "print the version information");
    opts.optopt("c", "config", "use a custom configuration file", "FILE");

    // Parse cmdline
    let args: Vec<String> = std::env::args().collect();
    let program = args[0].clone();
    let flags = opts.parse(&args[1..]).unwrap_or_else(|f| panic!("{}", f));
    if flags.opt_present("h") {
        let brief = format!("{} {} - {}\n\nUsage: {} [options]", built_info::PKG_NAME,built_info::PKG_VERSION,built_info::PKG_DESCRIPTION,program);
        print!("{}", opts.usage(&brief));
        return None;
    }
    if flags.opt_present("v") {
        print!("{}\n", built_info::PKG_VERSION);
        return None;
    }

    // Load defaults
    let mut b = DEFAULTS
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
    } else {
        b = b.add_source(
            config::File::with_name(if cfg!(target_family = "windows") {
                todo!() // Should probably do something with the registry
            } else {
                "/etc/hardy/bpa.config"
            })
            .required(false),
        );
    }

    // Pull in environment vars
    b = b.add_source(config::Environment::with_prefix("HARDY_BPA"));

    // And parse...
    b.build()
        .expect("Failed to parse configuration")
        .try_deserialize()
        .expect("Failed to deserialize config")
}
