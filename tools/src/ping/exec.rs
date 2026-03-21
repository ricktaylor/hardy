use super::*;
use hardy_bpa::bpa::BpaRegistration;

/// Exit codes matching Linux/BSD ping conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    /// At least one response was received
    Success = 0,
    /// No responses received (100% packet loss)
    NoResponse = 1,
    /// Other error (connection failure, invalid arguments, etc.)
    Error = 2,
}

/// Returns true if the --cla value looks like a file path (plugin) rather
/// than a built-in CLA name.
fn is_plugin_path(cla: &str) -> bool {
    cla.contains('/')
        || cla.contains('\\')
        || cla.ends_with(".so")
        || cla.ends_with(".dylib")
        || cla.ends_with(".dll")
}

async fn exec_async(args: &Command) -> anyhow::Result<ExitCode> {
    if !args.quiet {
        eprintln!(
            "Pinging {} from {}",
            args.destination,
            args.source.as_ref().unwrap()
        );
    }

    let node_ids = [args.node_id()?].as_slice().try_into().unwrap();
    let bpa = hardy_bpa::bpa::Bpa::builder()
        .status_reports(true)
        .node_ids(node_ids)
        .build();

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

    if is_plugin_path(&args.cla) {
        exec_plugin_cla(args, &bpa).await
    } else {
        exec_builtin_cla(args, &bpa).await
    }
}

async fn exec_builtin_cla(args: &Command, bpa: &hardy_bpa::bpa::Bpa) -> anyhow::Result<ExitCode> {
    match args.cla.as_str() {
        "tcpclv4" => {}
        other => return Err(anyhow::anyhow!("Unknown built-in CLA: '{other}'")),
    }

    let mut tcpclv4_config = hardy_tcpclv4::config::Config {
        address: None,
        session_defaults: hardy_tcpclv4::config::SessionConfig {
            require_tls: false,
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
            tls_config.ca_certs = Some(ca_dir.clone());
        }
        tcpclv4_config.tls = Some(tls_config);
        tcpclv4_config.session_defaults.require_tls = true;
    }

    let cla = std::sync::Arc::new(
        hardy_tcpclv4::Cla::new(&tcpclv4_config)
            .map_err(|e| anyhow::anyhow!("Failed to create CLA '{}': {e}", args.cla))?,
    );

    cla.register(bpa, args.cla.clone(), None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start CLA '{}': {e}", args.cla))?;

    let peer_addr = if let Some(peer) = &args.peer {
        peer.parse()
            .map_err(|e| anyhow::anyhow!("Failed to parse peer address '{}': {e}", peer))?
    } else {
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

    // Add a route for the destination via the TCPCLv4 peer
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

    run_ping(args, bpa).await
}

#[cfg(feature = "dynamic-plugins")]
async fn exec_plugin_cla(args: &Command, bpa: &hardy_bpa::bpa::Bpa) -> anyhow::Result<ExitCode> {
    if args.peer.is_some() {
        eprintln!(
            "Warning: peer address argument is ignored when using a plugin CLA; \
             pass peer info via --cla-config instead"
        );
    }

    let config_json = args.cla_config.as_deref().unwrap_or("{}");
    let path = std::path::Path::new(&args.cla);

    let (_lib, cla) = unsafe { hardy_plugin_abi::host::load_cla_plugin(path, config_json) }
        .map_err(|e| anyhow::anyhow!("Failed to load CLA plugin: {e}"))?;

    // Register with BPA — the plugin's on_register() will call
    // sink.add_peer() if peer/peer-node are in the config, creating
    // the wildcard RIB entry needed for routing.
    bpa.register_cla("plugin0".to_string(), None, cla, None)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to register plugin CLA: {e}"))?;

    run_ping(args, bpa).await

    // _lib dropped here, after bpa.shutdown() in run_ping
}

#[cfg(not(feature = "dynamic-plugins"))]
async fn exec_plugin_cla(args: &Command, _bpa: &hardy_bpa::bpa::Bpa) -> anyhow::Result<ExitCode> {
    Err(anyhow::anyhow!(
        "CLA plugin '{}' requires the dynamic-plugins feature",
        args.cla
    ))
}

async fn run_ping(args: &Command, bpa: &hardy_bpa::bpa::Bpa) -> anyhow::Result<ExitCode> {
    let cancel_token = tokio_util::sync::CancellationToken::new();
    cancel::listen_for_cancel(&cancel_token);

    let stats = exec_inner(args, bpa, &cancel_token).await?;

    cancel_token.cancel();

    bpa.shutdown().await;

    if stats.received > 0 {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::NoResponse)
    }
}

async fn exec_inner(
    args: &Command,
    bpa: &dyn BpaRegistration,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<service::Statistics> {
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
    // Default wait time is one interval if not explicitly specified
    if !cancel_token.is_cancelled() && args.count.is_some() {
        let wait_time = args.wait.map(|w| *w).unwrap_or_else(|| *args.interval);

        if !args.quiet {
            eprintln!(
                "Waiting up to {} for responses...",
                humantime::format_duration(wait_time)
            );
        }
        if tokio::time::timeout(wait_time, service.wait_for_responses(cancel_token))
            .await
            .is_err()
            && !args.quiet
        {
            eprintln!("Timeout waiting for responses");
        }
    }

    // Print summary statistics
    service.print_summary();

    Ok(service.statistics())
}

pub fn exec(args: Command) -> ! {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Failed to build tokio runtime: {e}");
            std::process::exit(ExitCode::Error as i32);
        }
    };

    match runtime.block_on(exec_async(&args)) {
        Ok(exit_code) => std::process::exit(exit_code as i32),
        Err(e) => {
            eprintln!("Error: {e}");
            std::process::exit(ExitCode::Error as i32);
        }
    }
}
