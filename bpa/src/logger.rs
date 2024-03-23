use super::*;

pub fn init(config: &config::Config) {
    let log_level: String = settings::get_with_default(config, "log_level", "info")
        .expect("Failed to find 'log_level' in config");

    let log_level = log_level
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
