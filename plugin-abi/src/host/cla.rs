//! CLA plugin loading and apartment proxy for CLA Sink.
//!
//! See the [apartment pattern docs](super) for background.

use super::*;
use hardy_bpa::Bytes;
use hardy_bpa::cla::{self, ClaAddress, Error, Result, Sink};
use hardy_bpv7::eid::NodeId;
use libloading::Symbol;
use std::sync::Arc;
use tracing::info;

/// Entry point type for CLA factory functions.
#[allow(improper_ctypes_definitions)]
type ClaFactoryFn =
    unsafe extern "C" fn(*const std::ffi::c_char) -> PluginResult<Arc<dyn hardy_bpa::cla::Cla>>;

/// Entry point type for runtime factory functions.
type RuntimeFactoryFn = unsafe extern "C" fn() -> *mut tokio::runtime::Runtime;

/// Load a CLA plugin by file path and call its factory function.
///
/// Returns an `Arc<dyn Cla>` wrapped in an apartment proxy. The proxy
/// holds the `Library` handle internally, so the shared library stays
/// loaded for the lifetime of the CLA — callers don't need to manage
/// the library lifetime separately.
pub fn load_cla_plugin(
    path: &Path,
    config_json: &str,
) -> std::result::Result<Arc<dyn hardy_bpa::cla::Cla>, PluginLoadError> {
    info!("Loading CLA plugin: {}", path.display());
    // Safety: we verify the ABI token before calling any other symbol,
    // and the same-rustc-version constraint ensures type layout compatibility.
    let lib = unsafe { load_and_check(path)? };

    // Create the plugin's runtime first — must use the plugin's tokio
    // copy so worker threads have the plugin's TLS.
    let rt_factory: Symbol<RuntimeFactoryFn> = unsafe { lib.get(b"hardy_create_runtime") }
        .map_err(|_| PluginLoadError::MissingSymbol {
            path: path.to_path_buf(),
            symbol: "hardy_create_runtime".to_string(),
        })?;

    let rt_ptr = unsafe { rt_factory() };
    if rt_ptr.is_null() {
        return Err(PluginLoadError::InvalidConfig {
            reason: "Plugin failed to create tokio runtime".to_string(),
        });
    }
    let runtime = unsafe { *Box::from_raw(rt_ptr) };

    // Create the CLA
    let cla_factory: Symbol<ClaFactoryFn> =
        unsafe { lib.get(b"hardy_create_cla") }.map_err(|_| PluginLoadError::MissingSymbol {
            path: path.to_path_buf(),
            symbol: "hardy_create_cla".to_string(),
        })?;

    let config_cstr = CString::new(config_json).map_err(|_| PluginLoadError::InvalidConfig {
        reason: "config JSON contains null byte".to_string(),
    })?;

    match unsafe { cla_factory(config_cstr.as_ptr()) } {
        PluginResult::Ok(cla) => Ok(Arc::new(ClaProxy::new(cla, runtime, lib))),
        PluginResult::Err(code) => Err(PluginLoadError::FactoryFailed {
            path: path.to_path_buf(),
            symbol: "hardy_create_cla".to_string(),
            code,
        }),
    }
}

// --- Apartment proxy for CLA Sink ---

/// Messages sent from the proxy to the host's Sink dispatcher.
enum SinkMessage {
    Unregister {
        reply: flume::Sender<()>,
    },
    Dispatch {
        bundle: Bytes,
        peer_node: Option<NodeId>,
        peer_addr: Option<ClaAddress>,
        reply: flume::Sender<Result<()>>,
    },
    AddPeer {
        cla_addr: ClaAddress,
        node_ids: Vec<NodeId>,
        reply: flume::Sender<Result<bool>>,
    },
    RemovePeer {
        cla_addr: ClaAddress,
        reply: flume::Sender<Result<bool>>,
    },
}

/// Proxy `Sink` that forwards calls across a channel to the host's runtime.
struct ProxySink {
    tx: flume::Sender<SinkMessage>,
}

#[hardy_bpa::async_trait]
impl Sink for ProxySink {
    async fn unregister(&self) {
        let (reply, rx) = flume::bounded(1);
        if self.tx.send(SinkMessage::Unregister { reply }).is_ok() {
            let _ = rx.recv_async().await;
        }
    }

    async fn dispatch(
        &self,
        bundle: Bytes,
        peer_node: Option<&NodeId>,
        peer_addr: Option<&ClaAddress>,
    ) -> Result<()> {
        let (reply, rx) = flume::bounded(1);
        self.tx
            .send(SinkMessage::Dispatch {
                bundle,
                peer_node: peer_node.cloned(),
                peer_addr: peer_addr.cloned(),
                reply,
            })
            .map_err(|_| Error::Disconnected)?;
        rx.recv_async().await.map_err(|_| Error::Disconnected)?
    }

    async fn add_peer(&self, cla_addr: ClaAddress, node_ids: &[NodeId]) -> Result<bool> {
        let (reply, rx) = flume::bounded(1);
        self.tx
            .send(SinkMessage::AddPeer {
                cla_addr,
                node_ids: node_ids.to_vec(),
                reply,
            })
            .map_err(|_| Error::Disconnected)?;
        rx.recv_async().await.map_err(|_| Error::Disconnected)?
    }

    async fn remove_peer(&self, cla_addr: &ClaAddress) -> Result<bool> {
        let (reply, rx) = flume::bounded(1);
        self.tx
            .send(SinkMessage::RemovePeer {
                cla_addr: cla_addr.clone(),
                reply,
            })
            .map_err(|_| Error::Disconnected)?;
        rx.recv_async().await.map_err(|_| Error::Disconnected)?
    }
}

/// Wrap a real `Sink` in an apartment proxy. The dispatcher task runs on
/// the current (host) runtime.
fn wrap_sink(real_sink: Box<dyn Sink>) -> Box<dyn Sink> {
    let real_sink: Arc<dyn Sink> = real_sink.into();
    let (tx, rx) = flume::unbounded::<SinkMessage>();

    tokio::spawn(async move {
        while let Ok(msg) = rx.recv_async().await {
            match msg {
                SinkMessage::Unregister { reply } => {
                    real_sink.unregister().await;
                    let _ = reply.send(());
                    break;
                }
                SinkMessage::Dispatch {
                    bundle,
                    peer_node,
                    peer_addr,
                    reply,
                } => {
                    let result = real_sink
                        .dispatch(bundle, peer_node.as_ref(), peer_addr.as_ref())
                        .await;
                    let _ = reply.send(result);
                }
                SinkMessage::AddPeer {
                    cla_addr,
                    node_ids,
                    reply,
                } => {
                    let result = real_sink.add_peer(cla_addr, &node_ids).await;
                    let _ = reply.send(result);
                }
                SinkMessage::RemovePeer { cla_addr, reply } => {
                    let result = real_sink.remove_peer(&cla_addr).await;
                    let _ = reply.send(result);
                }
            }
        }
    });

    Box::new(ProxySink { tx })
}

// --- CLA apartment ---

/// Messages from the host to the plugin's apartment dispatcher.
enum ClaMessage {
    OnRegister {
        sink: Box<dyn Sink>,
        node_ids: Vec<NodeId>,
        reply: flume::Sender<()>,
    },
    OnUnregister {
        reply: flume::Sender<()>,
    },
    Forward {
        queue: Option<u32>,
        cla_addr: ClaAddress,
        bundle: Bytes,
        reply: flume::Sender<cla::Result<cla::ForwardBundleResult>>,
    },
}

/// CLA apartment: owns the plugin's runtime and dispatches all trait
/// calls onto it via channels. The plugin CLA runs entirely within this
/// apartment — it never needs to create its own runtime or worry about
/// cross-runtime issues.
///
/// Also holds the `Library` handle so the shared library stays loaded
/// for the lifetime of the CLA.
struct ClaProxy {
    tx: flume::Sender<ClaMessage>,
    queue_count: u32,
    // Kept alive — dropping this unloads the plugin shared library.
    _library: Library,
    // Plugin's runtime — shutdown_background is called on drop to avoid
    // the "cannot drop runtime in async context" panic.
    _runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for ClaProxy {
    fn drop(&mut self) {
        if let Some(rt) = self._runtime.take() {
            rt.shutdown_background();
        }
    }
}

impl ClaProxy {
    fn new(inner: Arc<dyn cla::Cla>, runtime: tokio::runtime::Runtime, library: Library) -> Self {
        let queue_count = inner.queue_count();
        let (tx, rx) = flume::unbounded::<ClaMessage>();

        // Spawn the CLA dispatcher on the plugin's runtime.
        // All calls are serialized through the dispatcher.
        runtime.spawn(async move {
            while let Ok(msg) = rx.recv_async().await {
                match msg {
                    ClaMessage::OnRegister {
                        sink,
                        node_ids,
                        reply,
                    } => {
                        inner.on_register(sink, &node_ids).await;
                        let _ = reply.send(());
                    }
                    ClaMessage::OnUnregister { reply } => {
                        inner.on_unregister().await;
                        let _ = reply.send(());
                        break;
                    }
                    ClaMessage::Forward {
                        queue,
                        cla_addr,
                        bundle,
                        reply,
                    } => {
                        let result = inner.forward(queue, &cla_addr, bundle).await;
                        let _ = reply.send(result);
                    }
                }
            }
        });

        Self {
            tx,
            queue_count,
            _library: library,
            _runtime: Some(runtime),
        }
    }
}

#[hardy_bpa::async_trait]
impl cla::Cla for ClaProxy {
    async fn on_register(&self, sink: Box<dyn Sink>, node_ids: &[NodeId]) {
        // Wrap the Sink so calls back to the host cross the apartment boundary
        let proxy_sink = wrap_sink(sink);
        let (reply, rx) = flume::bounded(1);
        if self
            .tx
            .send(ClaMessage::OnRegister {
                sink: proxy_sink,
                node_ids: node_ids.to_vec(),
                reply,
            })
            .is_ok()
        {
            let _ = rx.recv_async().await;
        }
    }

    async fn on_unregister(&self) {
        let (reply, rx) = flume::bounded(1);
        if self.tx.send(ClaMessage::OnUnregister { reply }).is_ok() {
            let _ = rx.recv_async().await;
        }
    }

    fn queue_count(&self) -> u32 {
        self.queue_count
    }

    async fn forward(
        &self,
        queue: Option<u32>,
        cla_addr: &ClaAddress,
        bundle: Bytes,
    ) -> cla::Result<cla::ForwardBundleResult> {
        let (reply, rx) = flume::bounded(1);
        self.tx
            .send(ClaMessage::Forward {
                queue,
                cla_addr: cla_addr.clone(),
                bundle,
                reply,
            })
            .map_err(|_| cla::Error::Disconnected)?;
        rx.recv_async()
            .await
            .map_err(|_| cla::Error::Disconnected)?
    }
}
