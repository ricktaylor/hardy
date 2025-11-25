use super::*;

async fn start_bpa(args: &Command) -> anyhow::Result<hardy_bpa::bpa::Bpa> {
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
        ..Default::default()
    };
    
    // Configure TLS if accept_self_signed or CA bundle is specified
    if args.tls_accept_self_signed || args.tls_ca_bundle.is_some() {
        tcpclv4_config.session_defaults.use_tls = true;
        if args.tls_accept_self_signed {
            tcpclv4_config.tls.debug.accept_self_signed = true;
        }
        if let Some(ca_bundle) = &args.tls_ca_bundle {
            if !ca_bundle.exists() {
                return Err(anyhow::anyhow!("CA bundle directory not found: {}", ca_bundle.display()));
            }
            if !ca_bundle.is_dir() {
                return Err(anyhow::anyhow!("CA bundle must be a directory, not a file: {}", ca_bundle.display()));
            }
            tcpclv4_config.tls.ca_bundle = Some(ca_bundle.clone());
        }
    }
    
    let cla = std::sync::Arc::new(hardy_tcpclv4::Cla::new(
        cla_name.clone(),
        tcpclv4_config,
    ));

    bpa.register_cla(cla_name.clone(), None, cla.clone(), None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start CLA '{}': {e}", cla_name))?;

    let Some(address) = &args.address else {
        // TODO: DNS resolution for EIDs
        // https://datatracker.ietf.org/doc/draft-ek-dtn-ipn-arpa/
        return Err(anyhow::anyhow!(
            "No CLA address specified for destination EID, and no DNS support currently available"
        ));
    };

    let peer = match &args.destination {
        Eid::LegacyIpn {
            allocator_id,
            node_number,
            ..
        }
        | Eid::Ipn {
            allocator_id,
            node_number,
            ..
        } => Eid::Ipn {
            allocator_id: *allocator_id,
            node_number: *node_number,
            service_number: 0,
        },
        Eid::Dtn { node_name, .. } => Eid::Dtn {
            node_name: node_name.clone(),
            demux: "".into(),
        },
        eid => {
            return Err(anyhow::anyhow!(
                "Invalid source EID '{eid}' for ping service"
            ));
        }
    };

    cla.add_peer(
        address
            .parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse CLA address: {e}"))?,
        peer.clone(),
    )
    .await
    .then_some(())
    .ok_or(anyhow::anyhow!("Failed to add peer to CLA"))?;

    // Now add a route
    bpa.add_route(
        "ping".to_string(),
        args.destination.clone().into(),
        hardy_bpa::routes::Action::Via(peer),
        1,
    )
    .await
    .then_some(bpa)
    .ok_or(anyhow::anyhow!("Failed to add route"))
}

async fn exec_async(args: &Command) -> anyhow::Result<()> {
    println!(
        "Pinging {} from {}",
        args.destination,
        args.source.as_ref().unwrap()
    );

    let bpa = start_bpa(args).await?;

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
    let service_id = match args.source.as_ref().unwrap() {
        Eid::LegacyIpn {
            allocator_id: _,
            node_number: _,
            service_number,
        }
        | Eid::Ipn {
            allocator_id: _,
            node_number: _,
            service_number,
        } => hardy_bpa::service::ServiceId::IpnService(*service_number),
        Eid::Dtn {
            node_name: _,
            demux,
        } => hardy_bpa::service::ServiceId::DtnService(demux),
        eid => {
            return Err(anyhow::anyhow!(
                "Invalid source EID '{eid}' for ping service"
            ));
        }
    };

    let service = std::sync::Arc::new(service::Service::new(args));
    bpa.register_service(Some(service_id), service.clone())
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
