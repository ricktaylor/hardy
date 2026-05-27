use core::time::Duration;

use flume::Sender;
use hardy_async::CancellationToken;
use hardy_bpv7::bundle::Id as BundleId;
use hardy_bpv7::eid::Eid;

use super::{Bytes, Error, Result, SendOptions};

pub enum ServiceOp {
    SendRaw {
        data: Bytes,
        reply: Sender<Result<BundleId>>,
    },
    Send {
        destination: Eid,
        data: Bytes,
        lifetime: Duration,
        options: Option<SendOptions>,
        reply: Sender<Result<BundleId>>,
    },
    Cancel {
        bundle_id: BundleId,
        reply: Sender<Result<bool>>,
    },
}

#[derive(Clone)]
struct ContextInner {
    ops: Sender<ServiceOp>,
    endpoint: Eid,
    shutdown: CancellationToken,
}

impl ContextInner {
    async fn cancel(&self, bundle_id: BundleId) -> Result<bool> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.ops
            .send_async(ServiceOp::Cancel {
                bundle_id,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Disconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| Error::Disconnected)?
    }
}

/// Context for low-level services that send raw bundle bytes.
#[derive(Clone)]
pub struct ServiceContext {
    inner: ContextInner,
}

impl ServiceContext {
    pub fn new(ops: Sender<ServiceOp>, endpoint: Eid, shutdown: CancellationToken) -> Self {
        Self {
            inner: ContextInner {
                ops,
                endpoint,
                shutdown,
            },
        }
    }

    pub fn endpoint(&self) -> &Eid {
        &self.inner.endpoint
    }

    pub async fn send_raw(&self, data: Bytes) -> Result<BundleId> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .ops
            .send_async(ServiceOp::SendRaw {
                data,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Disconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| Error::Disconnected)?
    }

    pub async fn cancel(&self, bundle_id: BundleId) -> Result<bool> {
        self.inner.cancel(bundle_id).await
    }

    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.inner.shutdown
    }

    pub fn is_connected(&self) -> bool {
        !self.inner.ops.is_disconnected()
    }
}

/// Context for high-level applications that send payloads with destination and options.
#[derive(Clone)]
pub struct AppContext {
    inner: ContextInner,
}

impl AppContext {
    pub fn new(ops: Sender<ServiceOp>, endpoint: Eid, shutdown: CancellationToken) -> Self {
        Self {
            inner: ContextInner {
                ops,
                endpoint,
                shutdown,
            },
        }
    }

    pub fn endpoint(&self) -> &Eid {
        &self.inner.endpoint
    }

    pub async fn send(
        &self,
        destination: Eid,
        data: Bytes,
        lifetime: Duration,
        options: Option<SendOptions>,
    ) -> Result<BundleId> {
        let (reply_tx, reply_rx) = flume::bounded(1);
        self.inner
            .ops
            .send_async(ServiceOp::Send {
                destination,
                data,
                lifetime,
                options,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Error::Disconnected)?;
        reply_rx
            .recv_async()
            .await
            .map_err(|_| Error::Disconnected)?
    }

    pub async fn cancel(&self, bundle_id: BundleId) -> Result<bool> {
        self.inner.cancel(bundle_id).await
    }

    pub fn shutdown_token(&self) -> &CancellationToken {
        &self.inner.shutdown
    }

    pub fn is_connected(&self) -> bool {
        !self.inner.ops.is_disconnected()
    }
}
