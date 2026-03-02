use super::*;

#[cfg(feature = "echo")]
pub async fn init(
    services: Option<&[hardy_bpv7::eid::Service]>,
    bpa: &dyn hardy_bpa::bpa::BpaRegistration,
) {
    let Some(services) = services else {
        return;
    };
    if services.is_empty() {
        warn!("Echo service configured but no endpoints specified, skipping registration");
        return;
    }
    info!(
        "Registering echo service on {} endpoint(s): {:?}",
        services.len(),
        services
    );
    let echo = Arc::new(hardy_echo_service::EchoService::new());
    match echo.register(bpa, services).await {
        Ok(eids) => {
            for eid in eids {
                info!("Echo service registered at {eid}");
            }
        }
        Err(e) => error!("Failed to register echo service: {e}"),
    }
}

#[cfg(not(feature = "echo"))]
pub async fn init(
    services: Option<&[hardy_bpv7::eid::Service]>,
    _bpa: &dyn hardy_bpa::bpa::BpaRegistration,
) {
    if services.is_some() {
        warn!("Ignoring built-in-services.echo configuration: echo feature is disabled at compile time");
    }
}
