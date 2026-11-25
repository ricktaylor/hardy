use super::*;
use hardy_bpa::{
    cla::RegistrarExt,
    routing::RegistrarExt as _,
    service::{Registrar, RegistrarExt as _},
};

// Exit codes matching Linux/BSD ping conventions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitCode {
    // At least one response was received.
    Success = 0,
    // No response was received: 100% packet loss.
    NoResponse = 1,
    // Anything else: a connection failure, invalid arguments.
    Error = 2,
}

// Whether the `--cla` value looks like a path to an external binary
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

    // bp is a diagnostic client, so it accepts echoes from peers that do
    // not meet every RFC 9171 ingress policy, notably dtn7-rs's dtnecho2,
    // which reflects bundles without a primary-block CRC. `--lax-rfc9171`
    // relaxes the BPA's config-gated ingress checks, and the reflected
    // payload is still compared byte for byte by the client.
    let bpa = alloc::sync::Arc::new(
        hardy_bpa::bpa::Bpa::builder()
            .status_reports(true)
            .node_ids(node_ids)
            .primary_block_integrity(!args.lax_rfc9171)
            .bundle_age_required(!args.lax_rfc9171)
            .build(),
    );

    // A default `Drop` route keeps the client from caching bundles
    // locally.
    bpa.register_routing_agent(
        "ping",
        hardy_bpa::routing::StaticAgent::factory(&[(
            "*:**".parse().unwrap(),
            hardy_bpa::routing::RouteAction::Drop(Some(
                hardy_bpv7::status_report::ReasonCode::NoKnownRouteToDestinationFromHere,
            )),
            1000,
        )]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Failed to register default routes: {e}"))?;

    bpa.start(false)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to start BPA: {e}"))?;

    if is_external_cla(&args.cla) {
        exec_external_cla(args, &bpa).await
    } else {
        exec_builtin_cla(args, &bpa).await
    }
}

async fn exec_builtin_cla(
    args: &Command,
    bpa: &alloc::sync::Arc<hardy_bpa::bpa::Bpa>,
) -> anyhow::Result<ExitCode> {
    match args.cla.as_str() {
        "tcpclv4" => {}
        other => return Err(anyhow::anyhow!("Unknown built-in CLA: '{other}'")),
    }

    // ping only dials, so it never claims the registered listening
    // port.
    let mut builder = hardy_tcpclv4::Tcpclv4::builder();

    // TLS is required as soon as `--tls-insecure` or `--tls-ca` asks for
    // it, and the two flags conflict at argument parsing, so at most one is
    // set. A directory problem surfaces from `build` with the path
    // named.
    if args.tls_insecure {
        builder = builder.tls(
            hardy_tcpclv4::tls::Tls::builder()
                .dangerous()
                .insecure_skip_verify()
                .required(true),
        );
    } else if let Some(ca_dir) = &args.tls_ca {
        builder = builder.tls(
            hardy_tcpclv4::tls::Tls::builder()
                .ca_certs(ca_dir.clone())
                .required(true),
        );
    }

    let factory = builder
        .build()
        .map_err(|e| anyhow::anyhow!("Failed to create CLA '{}': {e}", args.cla))?;

    let cla = bpa
        .register_cla(&args.cla, factory)
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

    // `connect` registers the session from a spawned task.
    // TODO: wait on a notification instead of sleeping.
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let peer: NodeId = args.destination.clone().try_to_node_id().map_err(|_| {
        anyhow::anyhow!(
            "Invalid destination EID {} for ping service",
            args.destination
        )
    })?;

    // A service destination needs a route of its own.
    if args.destination.service().is_some() {
        bpa.register_routing_agent(
            "ping-target",
            hardy_bpa::routing::StaticAgent::factory(&[(
                args.destination.clone().into(),
                hardy_bpa::routing::RouteAction::Via(peer.into()),
                1,
            )]),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to add route: {e}"))?;
    }

    run_ping(args, bpa).await
}

async fn exec_external_cla(
    args: &Command,
    bpa: &alloc::sync::Arc<hardy_bpa::bpa::Bpa>,
) -> anyhow::Result<ExitCode> {
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

    // Yield so the gRPC server task binds and starts listening.
    tokio::task::yield_now().await;

    let mut cmd = tokio::process::Command::new(&args.cla);

    if let Some(cla_args) = &args.cla_args {
        for arg in cla_args.split_whitespace() {
            cmd.arg(arg);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to start CLA process '{}': {e}", args.cla))?;

    // The CLA subprocess connects over gRPC and registers asynchronously,
    // so wait until it has fully registered and its `add_peer` has created
    // the forward entries the first ping needs.
    // TODO: wait on a routing agent notification API when one exists.
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

    // The CLA's `add_peer` creates a forward entry, but resolving the
    // destination EID to the peer node still needs a route.
    if args.destination.service().is_some() {
        let peer: NodeId = args.destination.clone().try_to_node_id().map_err(|_| {
            anyhow::anyhow!(
                "Invalid destination EID {} for ping service",
                args.destination
            )
        })?;

        bpa.register_routing_agent(
            "ping-target",
            hardy_bpa::routing::StaticAgent::factory(&[(
                args.destination.clone().into(),
                hardy_bpa::routing::RouteAction::Via(peer.into()),
                1,
            )]),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to add route: {e}"))?;
    }

    let result = run_ping(args, bpa).await;

    // The CLA subprocess is killed before the gRPC server is stopped:
    // tonic's `serve_with_shutdown` waits for active connections to close,
    // and the CLA holds an active gRPC stream, so the reverse order
    // hangs.
    let _ = child.kill().await;
    tasks.shutdown().await;

    result
}

async fn run_ping(
    args: &Command,
    bpa: &alloc::sync::Arc<hardy_bpa::bpa::Bpa>,
) -> anyhow::Result<ExitCode> {
    let tasks = hardy_async::TaskPool::new();
    hardy_async::signal::listen_for_cancel(&tasks);

    // Set by the deadline task alone, so an expired deadline can be told
    // apart from an operator's Ctrl+C when choosing the exit code. Both
    // cancel the same token, but only the former is a failure.
    let timed_out = alloc::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // A session deadline is a scheduled Ctrl+C, so drive it through the same
    // cancellation token the session already observes. Racing the session future
    // against a sleep would instead drop it wherever it was suspended, leaving a
    // bundle on the wire but never recorded. Waiting on the token rather than
    // sleeping bare also lets this task exit as soon as the pool shuts down
    // after a normal run.
    if let Some(t) = args.timeout {
        let cancel = tasks.cancel_token().clone();
        let timed_out = timed_out.clone();
        hardy_async::spawn!(tasks, "ping_deadline", async move {
            if tokio::time::timeout(*t, cancel.cancelled()).await.is_err() {
                timed_out.store(true, std::sync::atomic::Ordering::Relaxed);
                cancel.cancel();
            }
        });
    }

    let service = exec_inner(args, bpa.as_ref(), tasks.cancel_token()).await?;

    let stats = service.statistics();
    service.print_summary(&stats);
    tasks.shutdown().await;

    bpa.shutdown().await;

    // iputils `ping -c N -w D` exits 1 when the deadline arrives having received
    // fewer than N replies, so a run the deadline truncated is a failure even
    // when everything it managed to send was answered.
    let truncated = timed_out.load(std::sync::atomic::Ordering::Relaxed)
        && args.count.is_some_and(|count| stats.received < count);

    if stats.received > 0 && !truncated {
        Ok(ExitCode::Success)
    } else {
        Ok(ExitCode::NoResponse)
    }
}

async fn exec_inner(
    args: &Command,
    bpa: &dyn Registrar,
    cancel_token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<alloc::sync::Arc<service::Service>> {
    let service = bpa
        .register_raw_service(
            args.source.as_ref().and_then(|eid| eid.service()),
            service::Service::factory(args),
        )
        .await
        .map_err(|e| anyhow::anyhow!("Failed to register service: {e}"))?;

    for seq_no in 0..args.count.unwrap_or(u32::MAX) {
        service.send(args, seq_no).await?;

        if tokio::time::timeout(*args.interval, cancel_token.cancelled())
            .await
            .is_ok()
        {
            break;
        }
    }

    // Wait for the outstanding responses once every ping has been sent,
    // for one interval unless `--wait` says otherwise.
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
    Ok(service)
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
