use super::*;
use hardy_bpa::bpa::BpaRegistration;

// Exit codes matching Linux/BSD ping conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    // At least one response was received
    Success = 0,
    // No responses received (100% packet loss)
    NoResponse = 1,
    // Other error (connection failure, invalid arguments, etc.)
    Error = 2,
}

// Returns true if the --cla value looks like a file path (external binary)
// rather than a built-in CLA name.
fn is_external_cla(cla: &str) -> bool {
    cla.contains('/') || cla.contains('\\')
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
    let bpa = std::sync::Arc::new(
        hardy_bpa::bpa::Bpa::builder()
            .status_reports(true)
            .node_ids(node_ids)
            .build()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to build BPA: {e}"))?,
    );

    // Add a default 'drop' route, we don't want to cache locally
    bpa.register_routing_agent(
        "ping".to_string(),
        std::sync::Arc::new(hardy_bpa::routes::StaticRoutingAgent::new(&[(
            "*:**".parse().unwrap(),
            hardy_bpa::routes::Action::Drop(Some(
                hardy_bpv7::status_report::ReasonCode::NoKnownRouteToDestinationFromHere,
            )),
            1000,
        )])),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to register default routes: {e}"))?;

    bpa.start(false);

    if is_external_cla(&args.cla) {
        exec_external_cla(args, &bpa).await
    } else {
        exec_builtin_cla(args, &bpa).await
    }
}

async fn exec_builtin_cla(
    args: &Command,
    bpa: &std::sync::Arc<hardy_bpa::bpa::Bpa>,
) -> anyhow::Result<ExitCode> {
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

    bpa.register_cla(args.cla.clone(), cla.clone(), None)
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

    // Now add a route if we are targeting a service
    if args.destination.service().is_some() {
        bpa.register_routing_agent(
            "ping-target".to_string(),
            std::sync::Arc::new(hardy_bpa::routes::StaticRoutingAgent::new(&[(
                args.destination.clone().into(),
                hardy_bpa::routes::Action::Via(peer.into()),
                1,
            )])),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to add route: {e}"))?;
    }

    run_ping(args, bpa).await
}

async fn exec_external_cla(
    args: &Command,
    bpa: &std::sync::Arc<hardy_bpa::bpa::Bpa>,
) -> anyhow::Result<ExitCode> {
    // Start gRPC server with CLA service
    let tasks = hardy_async::TaskPool::new();
    let grpc_config = hardy_proto::server::Config {
        address: args.grpc_listen,
        services: vec!["cla".to_string()],
    };
    let server = hardy_proto::server::GrpcServer::new(&grpc_config, bpa.clone())
        .map_err(|e| anyhow::anyhow!("Failed to create gRPC server: {e}"))?;
    let cancel = tasks.cancel_token().clone();
    hardy_async::spawn!(tasks, "grpc_server", async move {
        if let Err(e) = server.serve(cancel).await {
            tracing::error!("gRPC server failed: {e}");
        }
    });

    // Yield to let the gRPC server task bind and start listening
    tokio::task::yield_now().await;

    // Spawn the CLA binary as a subprocess
    let mut cmd = tokio::process::Command::new(&args.cla);

    // Pass through user-supplied arguments
    if let Some(cla_args) = &args.cla_args {
        for arg in cla_args.split_whitespace() {
            cmd.arg(arg);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to start CLA process '{}': {e}", args.cla))?;

    // Wait for the CLA to fully register (including add_peer creating forward
    // entries). The CLA subprocess connects via gRPC and the registration
    // completes asynchronously — we need the forward entries to exist before
    // we start sending pings.
    // TODO: Replace with proper RoutingAgent notification API when available
    {
        let timeout = tokio::time::sleep(std::time::Duration::from_secs(10));
        tokio::pin!(timeout);
        tokio::select! {
            _ = &mut timeout => {
                let _ = child.kill().await;
                return Err(anyhow::anyhow!("CLA didn't start within 10 seconds"));
            }
            status = child.wait() => {
                return Err(anyhow::anyhow!(
                    "CLA process exited unexpectedly: {}",
                    status?
                ));
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                if !args.quiet {
                    eprintln!("External CLA started");
                }
            }
        }
    }

    // Add a route for the destination via the peer node
    // (the CLA's add_peer creates a forward entry, but the BPA also needs
    // a route to resolve the destination EID to the peer node)
    if args.destination.service().is_some() {
        let peer: NodeId = args.destination.clone().try_to_node_id().map_err(|_| {
            anyhow::anyhow!(
                "Invalid destination EID {} for ping service",
                args.destination
            )
        })?;

        bpa.register_routing_agent(
            "ping-target".to_string(),
            std::sync::Arc::new(hardy_bpa::routes::StaticRoutingAgent::new(&[(
                args.destination.clone().into(),
                hardy_bpa::routes::Action::Via(peer.into()),
                1,
            )])),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to add route: {e}"))?;
    }

    let result = run_ping(args, bpa).await;

    // Clean up: kill CLA subprocess first, then stop gRPC server.
    // Order matters: gRPC server shutdown (tonic serve_with_shutdown) waits
    // for active connections to close. The CLA holds an active gRPC stream,
    // so the server would hang if we tried to shut it down first.
    let _ = child.kill().await;
    tasks.shutdown().await;

    result
}

async fn run_ping(
    args: &Command,
    bpa: &std::sync::Arc<hardy_bpa::bpa::Bpa>,
) -> anyhow::Result<ExitCode> {
    let tasks = hardy_async::TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    let stats = exec_inner(args, bpa.as_ref(), tasks.cancel_token()).await?;

    tasks.shutdown().await;

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
    if let Some(service_id) = args.source.as_ref().and_then(|eid| eid.service()) {
        bpa.register_service(service_id, service.clone()).await
    } else {
        bpa.register_dynamic_service(service.clone()).await
    }
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
