use super::*;

pub fn init(config: &settings::Config) {
    simplelog::TermLogger::init(
        config
            .log_level
            .parse::<simplelog::LevelFilter>()
            .log_expect("Invalid log level"),
        simplelog::Config::default(),
        simplelog::TerminalMode::Mixed,
        simplelog::ColorChoice::Auto,
    )
    .expect("Failed to configure logging")
}
