use super::*;

async fn exec_async(args: &Command) -> anyhow::Result<()> {
    if !args.quiet {
        eprintln!(
            "Pinging {} from {}",
            args.destination,
            args.source.as_ref().unwrap()
        );
    }

    let bpa = hardy_bpa::bpa::Bpa::new(&hardy_bpa::config::Config {
        status_reports: true,
        node_ids: [args.node_id()?].as_slice().try_into().unwrap(),
        ..Default::default()
    });

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

    bpa.start(false);

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

    // Configure TLS if --tls-insecure or --tls-ca is specified
    if args.tls_insecure || args.tls_ca.is_some() {
        let mut tls_config = hardy_tcpclv4::config::TlsConfig::default();
        if args.tls_insecure {
            tls_config.debug.accept_self_signed = true;
        }
        if let Some(ca_dir) = &args.tls_ca {
            if !ca_dir.exists() {
                return Err(anyhow::anyhow!(
                    "CA bundle directory not found: {}",
                    ca_dir.display()
                ));
            }
            if !ca_dir.is_dir() {
                return Err(anyhow::anyhow!(
                    "CA bundle must be a directory, not a file: {}",
                    ca_dir.display()
                ));
            }
            tls_config.ca_bundle = Some(ca_dir.clone());
        }
        tcpclv4_config.tls = Some(tls_config);
        tcpclv4_config.session_defaults.must_use_tls = true;
    }

    let cla = std::sync::Arc::new(
        hardy_tcpclv4::Cla::new(&tcpclv4_config)
            .map_err(|e| anyhow::anyhow!("Failed to create CLA '{cla_name}': {e}"))?,
    );

    cla.register(&bpa, cla_name.clone(), None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start CLA '{cla_name}': {e}"))?;

    let peer_addr = if let Some(peer) = &args.peer {
        peer.parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse peer address '{}': {e}", peer))?
    } else {
        // TODO: DNS resolution for EIDs
        // https://datatracker.ietf.org/doc/draft-ek-dtn-ipn-arpa/

        return Err(anyhow::anyhow!(
            "No peer address specified for destination EID, and no DNS support currently available"
        ));
    };

    cla.connect(&peer_addr)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to connect to {peer_addr}: {e}"))?;
    drop(cla);

    // Wait for session registration to complete (async task spawned by connect)
    // TODO: This should be a proper wait/notification mechanism
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let peer: NodeId = args.destination.clone().try_to_node_id().map_err(|_| {
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

    // Wait for responses after sending all pings
    if !cancel_token.is_cancelled()
        && args.count.is_some()
        && let Some(wait) = &args.wait
    {
        if !args.quiet {
            eprintln!(
                "Waiting up to {} for responses...",
                humantime::format_duration(**wait)
            );
        }
        if tokio::time::timeout(**wait, service.wait_for_responses(cancel_token))
            .await
            .is_err()
            && !args.quiet
        {
            eprintln!("Timeout waiting for responses");
        }
    }

    // Print summary statistics
    service.print_summary();

    Ok(())
}

pub fn exec(args: Command) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to build tokio runtime: {e}"))?
        .block_on(exec_async(&args))
}
