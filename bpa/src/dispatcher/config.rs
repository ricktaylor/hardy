use super::*;
use utils::settings;

const MAX_FORWARDING_DELAY_SECS: u32 = 5;

#[derive(Clone)]
pub struct Config {
    pub admin_endpoints: utils::admin_endpoints::AdminEndpoints,
    pub status_reports: bool,
    pub wait_sample_interval: u64,
    pub max_forwarding_delay: u32,
    pub ipn_2_element: bpv7::EidPatternMap<(), ()>,
}

impl Config {
    pub fn new(
        config: &::config::Config,
        admin_endpoints: utils::admin_endpoints::AdminEndpoints,
    ) -> Self {
        let config = Self {
            admin_endpoints,
            status_reports: settings::get_with_default(config, "status_reports", false)
                .trace_expect("Invalid 'status_reports' value in configuration"),
            wait_sample_interval: settings::get_with_default(
                config,
                "wait_sample_interval",
                settings::WAIT_SAMPLE_INTERVAL_SECS,
            )
            .trace_expect("Invalid 'wait_sample_interval' value in configuration"),
            max_forwarding_delay: settings::get_with_default::<u32, _>(
                config,
                "max_forwarding_delay",
                MAX_FORWARDING_DELAY_SECS,
            )
            .trace_expect("Invalid 'max_forwarding_delay' value in configuration")
            .min(1u32),
            ipn_2_element: Self::load_ipn_2_element(config),
        };

        if !config.status_reports {
            info!("Bundle status reports are disabled by configuration");
        }

        if config.max_forwarding_delay == 0 {
            info!("Forwarding synchronization delay disabled by configuration");
        }

        config
    }

    fn load_ipn_2_element(config: &::config::Config) -> bpv7::EidPatternMap<(), ()> {
        let mut m = bpv7::EidPatternMap::new();
        for s in config
            .get::<Vec<String>>("ipn_2_element")
            .unwrap_or_default()
        {
            let p = s
                .parse::<bpv7::EidPattern>()
                .trace_expect(&format!("Invalid EID pattern '{s}"));
            m.insert(&p, (), ());
        }
        m
    }
}
