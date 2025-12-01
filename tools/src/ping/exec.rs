use super::*;

async fn exec_async(args: &Command) -> anyhow::Result<()> {
    println!(
        "Pinging {} from {}",
        args.destination,
        args.source.as_ref().unwrap()
    );

    let bpa = hardy_bpa::bpa::Bpa::start(
        &hardy_bpa::config::Config {
            status_reports: args.report_to.is_some(),
            node_ids: [args.node_id()?].as_slice().try_into().unwrap(),
            ..Default::default()
        },
        false,
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to start BPA: {e}"))?;

    // Add a default 'drop' route, we don't want to cache locally
    bpa.add_route(
        "ping".to_string(),
        "*:**".parse().unwrap(),
        hardy_bpa::routes::Action::Drop(Some(
            hardy_bpv7::status_report::ReasonCode::NoKnownRouteToDestinationFromHere,
        )),
        1000,
    )
    .await;

    // Register TCPCLv4 CLA
    let cla_name = "tcp0".to_string();
    let mut tcpclv4_config = hardy_tcpclv4::config::Config {
        address: None,
        session_defaults: hardy_tcpclv4::config::SessionConfig {
            must_use_tls: false,
            ..Default::default()
        },
        ..Default::default()
    };

    // Configure TLS if accept_self_signed or CA bundle is specified
    if args.tls_accept_self_signed || args.tls_ca_bundle.is_some() {
        let mut tls_config = hardy_tcpclv4::config::TlsConfig::default();
        if args.tls_accept_self_signed {
            tls_config.debug.accept_self_signed = true;
        }
        if let Some(ca_bundle) = &args.tls_ca_bundle {
            if !ca_bundle.exists() {
                return Err(anyhow::anyhow!(
                    "CA bundle directory not found: {}",
                    ca_bundle.display()
                ));
            }
            if !ca_bundle.is_dir() {
                return Err(anyhow::anyhow!(
                    "CA bundle must be a directory, not a file: {}",
                    ca_bundle.display()
                ));
            }
            tls_config.ca_bundle = Some(ca_bundle.clone());
        }
        tcpclv4_config.tls = Some(tls_config);
        tcpclv4_config.session_defaults.must_use_tls = true;
    }

    let cla = std::sync::Arc::new(hardy_tcpclv4::Cla::new(cla_name.clone(), tcpclv4_config));

    bpa.register_cla(cla_name.clone(), None, cla.clone(), None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_name))?;

    let address = if let Some(address) = &args.address {
        address
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse CLA address: {e}"))?
    } else {
        // TODO: DNS resolution for EIDs
        // https://datatracker.ietf.org/doc/draft-ek-dtn-ipn-arpa/

        return Err(anyhow::anyhow!(
            "No CLA address specified for destination EID, and no DNS support currently available"
        ));
    };

    cla.connect(&address)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {address}: {e}"))?;
    drop(cla);

    let peer: NodeId = args.destination.clone().try_into().map_err(|_| {
        anyhow::anyhow!(
            "Invalid destination EID {} for ping service",
            args.destination
        )
    })?;

    // Now add a route if we are targeting a service
    if args.destination.service().is_some()
        && !bpa
            .add_route(
                "ping".to_string(),
                args.destination.clone().into(),
                hardy_bpa::routes::Action::Via(peer.into()),
                1,
            )
            .await
    {
        return Err(anyhow::anyhow!("Failed to add route"));
    }

    let cancel_token = tokio_util::sync::CancellationToken::new();
    cancel::listen_for_cancel(&cancel_token);

    let r = exec_inner(args, &bpa, &cancel_token).await;

    // Stop waiting for cancel
    cancel_token.cancel();

    bpa.shutdown().await;

    r
}

async fn exec_inner(
    args: &Command,
    bpa: &hardy_bpa::bpa::Bpa,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<()> {
    let service = std::sync::Arc::new(service::Service::new(args));
    bpa.register_service(
        args.source.as_ref().and_then(|eid| eid.service()),
        service.clone(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to register service: {e}"))?;

    for seq_no in 0..args.count.unwrap_or(u32::MAX) {
        service.send(args, seq_no).await?;

        if tokio::time::timeout(*args.interval, cancel_token.cancelled())
            .await
            .is_ok()
        {
            // Cancelled
            break;
        }
    }

    if !cancel_token.is_cancelled()
        && args.count.is_some()
        && let Some(wait) = args.wait
    {
        if let Some(wait) = wait {
            println!(
                "Waiting up to {} for responses...",
                humantime::format_duration(*wait)
            );
            tokio::time::timeout(*wait, service.wait_for_responses(cancel_token))
                .await
                .map_err(|_| anyhow::anyhow!("Timeout waiting for responses"))?;
        } else {
            println!("Waiting for responses...");
            service.wait_for_responses(cancel_token).await;
        }
    }
    Ok(())
}

pub fn exec(args: Command) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build tokio runtime: {e}"))?
        .block_on(exec_async(&args))
}
