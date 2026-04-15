use std::path::PathBuf;

// Parsed command-line arguments for the BPA server.
pub struct Args {
    // Path to a custom configuration file, overriding the default location.
    pub config_file: Option<PathBuf>,
    // If set, upgrade the bundle store to the current schema before starting.
    pub upgrade_storage: bool,
    // If set, attempt to recover damaged records in the store before starting.
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

// Parse command-line arguments.
//
// Returns `None` if `--help` or `--version` was printed and the process should exit.
pub fn parse() -> Option<Args> {
    let opts = options();
    let argv: Vec<String> = std::env::args().collect();
    let flags = opts
        .parse(&argv[1..])
        .expect("Failed to parse command line args");

    if flags.opt_present("h") {
        let brief = format!(
            "{} {} - {}\n\nUsage: {} [options]",
            env!("CARGO_PKG_NAME"),
            env!("CARGO_PKG_VERSION"),
            env!("CARGO_PKG_DESCRIPTION"),
            argv[0]
        );
        print!("{}", opts.usage(&brief));
        return None;
    }

    if flags.opt_present("v") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return None;
    }

    Some(Args {
        config_file: flags.opt_str("config").map(PathBuf::from),
        upgrade_storage: flags.opt_present("u"),
        recover_storage: flags.opt_present("r"),
    })
}
