use super::*;

pub fn init(config: &config::Config) {
    let log_level = config
        .get_string("log_level")
        .expect("Failed to find 'log_level' in config")
        .parse::<simplelog::LevelFilter>()
        .expect("Invalid log level");

    simplelog::TermLogger::init(
        log_level,
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .expect("Failed to configure logging")
}
