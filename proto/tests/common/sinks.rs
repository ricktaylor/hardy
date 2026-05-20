//! Mock context helpers for testing.
#![allow(dead_code)]

use hardy_bpa::{cla, routes, services};

// ── RoutingContext mock helper ────────────────────────────────────────

pub fn mock_routing_context() -> (routes::RoutingContext, flume::Receiver<routes::RouteOp>) {
    let (tx, rx) = flume::unbounded();
    let token = hardy_async::CancellationToken::new();
    (routes::RoutingContext::new(tx, token), rx)
}

// ── ClaContext mock helper ────────────────────────────────────────────

pub fn mock_cla_context() -> cla::ClaContext {
    let (ingress_tx, _) = flume::unbounded();
    let (peer_tx, _) = flume::unbounded();
    let token = hardy_async::CancellationToken::new();
    cla::ClaContext::new(ingress_tx, peer_tx, token)
}

// ── ServiceContext mock helper ────────────────────────────────────────

pub fn mock_service_context(endpoint: hardy_bpv7::eid::Eid) -> services::ServiceContext {
    let (tx, _) = flume::unbounded();
    let token = hardy_async::CancellationToken::new();
    services::ServiceContext::new(tx, endpoint, token)
}
