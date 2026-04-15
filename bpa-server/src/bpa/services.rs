use super::*;

// Register all enabled built-in application services with the BPA.
#[allow(unused_variables)]
pub async fn register(
    config: &config::BuiltInServicesConfig,
    bpa: &dyn hardy_bpa::bpa::BpaRegistration,
) {
    #[cfg(feature = "echo")]
    if let Some(services) = &config.echo {
        if services.is_empty() {
            warn!("built-in-services.echo: no endpoints configured, skipping");
        } else {
            match hardy_echo_service::EchoService::register(bpa, services).await {
                Ok(eids) => {
                    for eid in eids {
                        info!("Echo service registered at {eid}");
                    }
                }
                Err(e) => error!("Failed to register echo service: {e}"),
            }
        }
    }

    #[cfg(not(feature = "echo"))]
    if config.echo.is_some() {
        warn!("Ignoring built-in-services.echo: echo feature is disabled at compile time");
    }
}
