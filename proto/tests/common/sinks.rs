//! Mock sink implementations for testing.
//!
//! Each mock tracks `unregister()` calls via an `AtomicBool`.
//! Other methods return success with no side effects.
#![allow(dead_code)]

use hardy_async::async_trait;
use hardy_bpa::{cla, routes, services};
use std::sync::atomic::{AtomicBool, Ordering};

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

// ── ServiceSink ───────────────────────────────────────────────────────

pub struct MockServiceSink {
    unregistered: AtomicBool,
}

impl MockServiceSink {
    pub fn new() -> Self {
        Self {
            unregistered: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl services::ServiceSink for MockServiceSink {
    async fn unregister(&self) {
        self.unregistered.store(true, Ordering::Relaxed);
    }

    async fn send(&self, _data: hardy_bpa::Bytes) -> services::Result<hardy_bpv7::bundle::Id> {
        Err(services::Error::Internal(
            "mock sink: send not implemented".into(),
        ))
    }

    async fn cancel(&self, _bundle_id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        Ok(true)
    }
}

// ── ApplicationSink ───────────────────────────────────────────────────

pub struct MockApplicationSink {
    unregistered: AtomicBool,
}

impl MockApplicationSink {
    pub fn new() -> Self {
        Self {
            unregistered: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl services::ApplicationSink for MockApplicationSink {
    async fn unregister(&self) {
        self.unregistered.store(true, Ordering::Relaxed);
    }

    async fn send(
        &self,
        _destination: hardy_bpv7::eid::Eid,
        _data: hardy_bpa::Bytes,
        _lifetime: core::time::Duration,
        _options: Option<services::SendOptions>,
    ) -> services::Result<hardy_bpv7::bundle::Id> {
        Err(services::Error::Internal(
            "mock sink: send not implemented".into(),
        ))
    }

    async fn cancel(&self, _bundle_id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        Ok(true)
    }
}
