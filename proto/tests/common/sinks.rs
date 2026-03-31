//! Mock sink implementations for testing.
//!
//! Each mock tracks `unregister()` calls via an `AtomicBool`.
//! Other methods return success with no side effects.

use hardy_async::async_trait;
use hardy_bpa::{cla, routes, services};
use hardy_bpv7::eid::NodeId;
use std::sync::atomic::{AtomicBool, Ordering};

// ── RoutingSink ───────────────────────────────────────────────────────

pub struct MockRoutingSink {
    unregistered: AtomicBool,
}

impl MockRoutingSink {
    pub fn new() -> Self {
        Self {
            unregistered: AtomicBool::new(false),
        }
    }

    pub fn is_unregistered(&self) -> bool {
        self.unregistered.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl routes::RoutingSink for MockRoutingSink {
    async fn unregister(&self) {
        self.unregistered.store(true, Ordering::Relaxed);
    }

    async fn add_route(
        &self,
        _pattern: hardy_eid_patterns::EidPattern,
        _action: routes::Action,
        _priority: u32,
    ) -> routes::Result<bool> {
        Ok(true)
    }

    async fn remove_route(
        &self,
        _pattern: &hardy_eid_patterns::EidPattern,
        _action: &routes::Action,
        _priority: u32,
    ) -> routes::Result<bool> {
        Ok(true)
    }
}

// ── CLA Sink ──────────────────────────────────────────────────────────

pub struct MockClaSink {
    unregistered: AtomicBool,
}

impl MockClaSink {
    pub fn new() -> Self {
        Self {
            unregistered: AtomicBool::new(false),
        }
    }

    pub fn is_unregistered(&self) -> bool {
        self.unregistered.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl cla::Sink for MockClaSink {
    async fn unregister(&self) {
        self.unregistered.store(true, Ordering::Relaxed);
    }

    async fn dispatch(
        &self,
        _bundle: hardy_bpa::Bytes,
        _peer_node: Option<&NodeId>,
        _peer_addr: Option<&cla::ClaAddress>,
    ) -> cla::Result<()> {
        Ok(())
    }

    async fn add_peer(
        &self,
        _cla_addr: cla::ClaAddress,
        _node_ids: &[NodeId],
    ) -> cla::Result<bool> {
        Ok(true)
    }

    async fn remove_peer(&self, _cla_addr: &cla::ClaAddress) -> cla::Result<bool> {
        Ok(true)
    }
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

    pub fn is_unregistered(&self) -> bool {
        self.unregistered.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl services::ServiceSink for MockServiceSink {
    async fn unregister(&self) {
        self.unregistered.store(true, Ordering::Relaxed);
    }

    async fn send(&self, _data: hardy_bpa::Bytes) -> services::Result<hardy_bpv7::bundle::Id> {
        unimplemented!("send not needed for lifecycle tests")
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

    pub fn is_unregistered(&self) -> bool {
        self.unregistered.load(Ordering::Relaxed)
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
        unimplemented!("send not needed for lifecycle tests")
    }

    async fn cancel(&self, _bundle_id: &hardy_bpv7::bundle::Id) -> services::Result<bool> {
        Ok(true)
    }
}
