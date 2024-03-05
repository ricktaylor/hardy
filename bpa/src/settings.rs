use serde::Deserialize;

// Buildtime info
mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub grpc_addr: String,
    pub grpc_port: u16,
}

fn options() -> getopts::Options {
    let mut opts = getopts::Options::new();
    opts.optflag("h", "help", "print this help menu")
        .optflag("v", "version", "print the version information")
        .optopt("c", "config", "use a custom configuration file", "FILE");
    opts
}

fn defaults() -> Vec<(&'static str, config::Value)> {
    vec! {
        ("grpc_addr","[::1]".into()),
        ("grpc_port",50051.into())
    }
}

pub fn init() -> Option<Config> {
    // Parse cmdline
    let opts = options();
    let args: Vec<String> = std::env::args().collect();
    let program = args[0].clone();
    let flags = opts.parse(&args[1..]).unwrap_or_else(|f| panic!("{}", f));
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
