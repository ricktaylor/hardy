use directories_next::ProjectDirs;
use lazy_static::lazy_static;
use serde::Deserialize;
use std::sync::OnceLock;

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

pub fn init() {
    INSTANCE
        .set(load())
        .expect("Configuration loaded more than once")
}

fn load() -> Config {
    // Load defaults
    let mut b = DEFAULTS.iter().fold(config::Config::builder(), |b,(k,v)|{
        b.set_default(k, v.clone())
            .expect("Invalid default config value")
    });

    // Add global config file path
    if let Ok(source) = std::env::var("HARDY_BPA_CONFIG_FILE") {
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

    // Pull in local user config file if it exists
    if let Some(p) = ProjectDirs::from("", "Hardy", "Hardy-Bpa") {
        b = b.add_source(config::File::from(p.config_dir()).required(false));
    }

    // Pull in cmdline args
    b = b.add_source(config::Environment::with_prefix("HARDY_BPA"));

    // And parse...
    b
        .build()
        .expect("Failed to parse configuration")
        .try_deserialize()
        .expect("Failed to deserialize config")
}
