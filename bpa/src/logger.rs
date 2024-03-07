use super::*;

pub fn init(config: &settings::Config) {
    // Check the filter level
    let level = config.log_level.to_ascii_lowercase();
    let level = if "warning".starts_with(&level) {
        simplelog::LevelFilter::Error
    } else if "info".starts_with(&level) {
        simplelog::LevelFilter::Info
    } else if "debug".starts_with(&level) {
        simplelog::LevelFilter::Debug
    } else if "trace".starts_with(&level) {
        simplelog::LevelFilter::Trace
    } else {
        simplelog::LevelFilter::Error
    };

    simplelog::TermLogger::init(
        level,
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .expect("Faield to configure logging")
}
