use super::*;

pub fn init(config: &config::Config) {
    let log_level: String = settings::get_with_default(config, "log_level", "info")
        .expect("Failed to find 'log_level' in config");

    let log_level = log_level
        .parse::<tracing_subscriber::filter::LevelFilter>()
        .expect("Invalid log level");

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(false)
        .init();
}
