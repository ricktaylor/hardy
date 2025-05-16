use super::*;

pub fn init(config: &config::Config) {
    let log_level = settings::get_with_default::<String, _>(config, "log_level", "info")
        .expect("Invalid 'log_level' value in configuration")
        .parse::<tracing_subscriber::filter::LevelFilter>()
        .expect("Invalid log level");

    tracing_subscriber::fmt()
        .with_max_level(log_level)
        .with_target(
            log_level > tracing_subscriber::filter::LevelFilter::from_level(tracing::Level::INFO),
        )
        .init();
}
