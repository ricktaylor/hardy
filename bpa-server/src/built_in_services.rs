use super::*;
use serde::{Deserialize, Serialize};
use validator::Validate;

#[derive(Serialize, Deserialize, Debug, Default, Validate)]
#[serde(default, rename_all = "kebab-case")]
pub struct BuiltInServicesConfig {
    /// Echo service: list of service identifiers (int = IPN, string = DTN).
    #[validate(length(min = 1))]
    pub echo: Option<Vec<hardy_bpv7::eid::Service>>,
}

pub async fn init(config: &BuiltInServicesConfig, bpa: &dyn hardy_bpa::bpa::BpaRegistration) {
    #[cfg(feature = "echo")]
    if let Some(services) = &config.echo {
        if services.is_empty() {
            warn!("built-in-services.echo: no endpoints configured, skipping");
        } else {
            let echo = Arc::new(hardy_echo_service::EchoService::new());
            for service in services {
                match bpa
                    .register_service(Some(service.clone()), echo.clone())
                    .await
                {
                    Ok(eid) => info!("Echo service registered at {eid}"),
                    Err(e) => error!("Failed to register echo service at {service}: {e}"),
                }
            }
        }
    }

    #[cfg(not(feature = "echo"))]
    if config.echo.is_some() {
        warn!("Ignoring built-in-services.echo: echo feature is disabled at compile time");
    }
}
