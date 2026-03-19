use super::*;

#[allow(unused_variables)]
pub async fn register(
    config: &config::BuiltInServicesConfig,
    bpa: &dyn hardy_bpa::BpaRegistration,
) {
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
