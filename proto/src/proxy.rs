use super::*;
use hardy_async::sync::spin::Mutex;
use std::collections::HashMap;

pub trait SendMsg {
    type Msg;

    fn compose(msg_id: u32, msg: Self::Msg) -> Self;
}

pub trait RecvMsg {
    type Msg;

    fn msg_id(&self) -> u32;
    fn msg(self) -> Result<Self::Msg, tonic::Status>;
}

impl<T> SendMsg for Result<T, tonic::Status>
where
    T: SendMsg,
{
    type Msg = T::Msg;

    fn compose(msg_id: u32, msg: Self::Msg) -> Self {
        Ok(T::compose(msg_id, msg))
    }
}

#[async_trait]
pub trait ProxyHandler: Send + Sync {
    type SMsg;
    type RMsg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg>;
    async fn on_close(&self);
}

/// Pending response map. `Some(map)` while the reader is alive; `None` after
/// the reader exits. `call()` checks this before inserting — if closed, it
/// returns immediately (reader is dead, no one to correlate the response).
type PendingMap<R> =
    Arc<Mutex<Option<HashMap<u32, tokio::sync::oneshot::Sender<Result<R, tonic::Status>>>>>>;

/// Writer half: reads from a channel and sends on the gRPC outbound stream.
///
/// This is a dedicated task that owns the outbound direction. Anyone can send
/// messages by cloning `write_tx`. Analogous to `tcpclv4::writer::SessionWriter`.
async fn writer_task<S>(
    mut write_rx: tokio::sync::mpsc::Receiver<S>,
    tx: tokio::sync::mpsc::Sender<S>,
    cancel: hardy_async::CancellationToken,
) {
    loop {
        tokio::select! {
            msg = write_rx.recv() => {
                match msg {
                    Some(msg) => {
                        if tx.send(msg).await.is_err() {
                            debug!("Outbound channel closed");
                            break;
                        }
                    }
                    None => break,
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

/// Reader half: reads from the gRPC inbound stream, dispatches responses to
/// pending callers and requests to handler tasks.
///
/// Acquires a handler-pool permit before each `stream.message()`. Responses
/// drop it; requests consume it to spawn a handler. Without pre-acquisition
/// a full pool would park the reader on `spawn(...).await`, blocking the
/// very responses whose completion frees the pool.
async fn reader_task<S, R>(
    mut stream: tonic::Streaming<R>,
    write_tx: tokio::sync::mpsc::Sender<S>,
    pending: PendingMap<R::Msg>,
    handler: Arc<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
    tasks: hardy_async::TaskPool,
    permits: Arc<tokio::sync::Semaphore>,
    cancel: hardy_async::CancellationToken,
) where
    R: RecvMsg + Send + 'static,
    R::Msg: Send + 'static,
    S: SendMsg + Send + 'static,
    S::Msg: Send + 'static,
{
    loop {
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => p.expect("semaphore closed unexpectedly"),
            _ = cancel.cancelled() => break,
        };

        let msg = tokio::select! {
            r = stream.message() => match r {
                Ok(Some(m)) => m,
                Ok(None) => { debug!("gRPC connection closed"); break; }
                Err(e) => { error!("gRPC connection failed: {e}"); break; }
            },
            _ = cancel.cancelled() => break,
        };

        let msg_id = msg.msg_id();
        let pending_sender = pending.lock().as_mut().and_then(|m| m.remove(&msg_id));
        if let Some(ret) = pending_sender {
            _ = ret.send(msg.msg());
            continue;
        }

        match msg.msg() {
            Ok(msg) => {
                let handler = handler.clone();
                let write_tx = write_tx.clone();
                hardy_async::spawn!(tasks, "rpc_proxy_handler", async move {
                    let _permit = permit;
                    if let Some(response) = handler.on_notify(msg).await {
                        _ = write_tx
                            .send(S::compose(msg_id, response))
                            .await
                            .inspect_err(|_| debug!("Response dropped (connection closed)"));
                    }
                });
            }
            Err(status) => warn!("{status}"),
        }
    }

    handler.on_close().await;

    // Close the pending map — fail any remaining calls and prevent new ones.
    // This signals to call() that the reader is dead without cancelling the
    // writer, so in-flight handler tasks can still send their responses.
    let pending_calls: Vec<_> = pending
        .lock()
        .take() // Close: None = no new inserts allowed
        .into_iter()
        .flatten()
        .collect();
    for (_, ret) in pending_calls {
        _ = ret.send(Err(tonic::Status::cancelled("Connection closed")));
    }

    // Drop our write_tx clone. The writer stays alive as long as handler
    // tasks hold their clones. When all handlers complete, write_rx closes
    // and the writer exits naturally.
    drop(write_tx);
}

pub type Sender<S> = tokio::sync::mpsc::Sender<S>;

#[allow(clippy::type_complexity)]
pub struct RpcProxy<S, R>
where
    R: RecvMsg + Send,
    R::Msg: Send,
    S: SendMsg + Send,
    S::Msg: Send,
{
    write_tx: tokio::sync::mpsc::Sender<S>,
    pending: PendingMap<R::Msg>,
    next_msg_id: Mutex<u32>,
    tasks: hardy_async::TaskPool,
    handler_tasks: hardy_async::TaskPool,
}

impl<S, R> RpcProxy<S, R>
where
    R: RecvMsg + Send + 'static,
    R::Msg: Send,
    S: SendMsg + Send + 'static,
    S::Msg: Send,
{
    /// Synchronous send-then-receive for the pre-proxy handshake phase.
    /// Used before `run()` is called, when the caller owns both halves directly.
    pub async fn send(
        channel_sender: &mut Sender<S>,
        channel_receiver: &mut tonic::Streaming<R>,
        msg: S::Msg,
    ) -> Result<Option<R::Msg>, tonic::Status> {
        if channel_sender.send(S::compose(0, msg)).await.is_err() {
            return Ok(None);
        }

        let msg = channel_receiver
            .message()
            .await?
            .ok_or(tonic::Status::unavailable("Server shut down"))?;

        if msg.msg_id() != 0 {
            Err(tonic::Status::aborted("Out of sequence response"))
        } else {
            msg.msg().map(Some)
        }
    }

    /// Synchronous receive-then-send for the pre-proxy handshake phase.
    /// Used before `run()` is called, when the caller owns both halves directly.
    pub async fn recv<F, Fut>(
        channel_sender: &mut Sender<S>,
        channel_receiver: &mut tonic::Streaming<R>,
        f: F,
    ) -> Result<(), tonic::Status>
    where
        F: FnOnce(R::Msg) -> Fut,
        Fut: Future<Output = Result<S::Msg, tonic::Status>>,
    {
        let msg = channel_receiver
            .message()
            .await?
            .ok_or(tonic::Status::unavailable("Server shut down"))?;

        let msg_id = msg.msg_id();
        let msg = f(msg.msg()?).await?;

        channel_sender
            .send(S::compose(msg_id, msg))
            .await
            .map_err(|e| tonic::Status::unavailable(format!("Server shut down: {e}")))
    }

    /// Start the proxy with split reader/writer tasks.
    ///
    /// The proxy creates its own task pools plus a semaphore that bounds
    /// concurrent handlers to `available_parallelism()`. Call `shutdown()`
    /// to await completion of all in-flight handlers.
    ///
    /// After this call, use `call()` to send messages and await responses.
    pub fn run(
        channel_sender: Sender<S>,
        channel_receiver: tonic::Streaming<R>,
        handler: Box<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
    ) -> Self {
        let tasks = hardy_async::TaskPool::new();
        let handler_tasks = hardy_async::TaskPool::new();
        // Held only by the reader task's clone plus any live handler's
        // OwnedSemaphorePermit; drops when the last reference is gone.
        let handler_permits = Arc::new(tokio::sync::Semaphore::new(
            hardy_async::available_parallelism().get(),
        ));
        let (write_tx, write_rx) = tokio::sync::mpsc::channel(16);
        let pending: PendingMap<R::Msg> = Arc::new(Mutex::new(Some(HashMap::new())));
        let cancel = tasks.cancel_token().clone();

        // Writer task: write_rx → gRPC outbound
        let writer_sender = channel_sender;
        let writer_cancel = cancel.clone();
        hardy_async::spawn!(tasks, "rpc_proxy_writer", async move {
            writer_task(write_rx, writer_sender, writer_cancel).await;
        });

        // Reader task: gRPC inbound → dispatch handlers, bounded by handler_permits
        let reader_write_tx = write_tx.clone();
        let reader_pending = pending.clone();
        let handler = Arc::from(handler);
        let reader_tasks = handler_tasks.clone();
        let reader_cancel = cancel.clone();
        hardy_async::spawn!(tasks, "rpc_proxy_reader", async move {
            reader_task(
                channel_receiver,
                reader_write_tx,
                reader_pending,
                handler,
                reader_tasks,
                handler_permits,
                reader_cancel,
            )
            .await;
        });

        Self {
            write_tx,
            pending,
            next_msg_id: Mutex::new(1),
            tasks,
            handler_tasks,
        }
    }

    /// Send a message and await the correlated response.
    ///
    /// The message is sent via the writer channel (non-blocking with respect
    /// to the reader task). A oneshot is registered in the pending map, keyed
    /// by msg_id. The reader task completes the oneshot when it sees the
    /// matching response.
    pub async fn call(&self, msg: S::Msg) -> Result<Option<R::Msg>, tonic::Status> {
        let msg_id = {
            let mut id = self.next_msg_id.lock();
            let current = *id;
            *id = id.wrapping_add(1);
            // Skip 0 — reserved for handshake send/recv
            if *id == 0 {
                *id = 1;
            }
            current
        };

        let (ret_tx, ret_rx) = tokio::sync::oneshot::channel();

        // Register the pending response before sending.
        // If the map is closed (reader exited), fail immediately.
        {
            let mut guard = self.pending.lock();
            let Some(map) = guard.as_mut() else {
                return Ok(None); // Reader dead, no one to correlate response
            };
            map.insert(msg_id, ret_tx);
        }

        // Send via writer channel
        if self.write_tx.send(S::compose(msg_id, msg)).await.is_err() {
            // Writer closed — clean up and return
            if let Some(map) = self.pending.lock().as_mut() {
                map.remove(&msg_id);
            }
            return Ok(None);
        }

        // Await the response
        let Ok(r) = ret_rx.await else {
            return Ok(None);
        };
        r.map(Some)
    }

    /// Cancel the proxy without awaiting task completion.
    ///
    /// Called when the BPA unregisters this component. Safe to call from
    /// any context, including from within a handler task. Tasks exit
    /// asynchronously.
    pub fn cancel(&self) {
        self.handler_tasks.cancel_token().cancel();
        self.tasks.cancel_token().cancel();
    }

    /// Shut down the proxy and await completion of all tasks.
    ///
    /// Drains the handler pool first (allowing in-flight handlers to send
    /// their responses via the still-active writer), then shuts down the
    /// infrastructure pool (reader + writer).
    ///
    /// Safe to call multiple times (idempotent). If the proxy is already
    /// cancelled (e.g., re-entrant call from within `on_close`), this
    /// returns immediately to avoid deadlock.
    pub async fn shutdown(&self) {
        if self.tasks.is_cancelled() {
            return;
        }
        self.handler_tasks.shutdown().await;
        self.tasks.shutdown().await;
    }
}

impl<S, R> Drop for RpcProxy<S, R>
where
    R: RecvMsg + Send,
    R::Msg: Send,
    S: SendMsg + Send,
    S::Msg: Send,
{
    fn drop(&mut self) {
        // Cancel tasks so the stream closes promptly. Matches the
        // "Drop = unregister" design principle — an abandoned proxy
        // should not leave orphaned tasks on the runtime.
        self.handler_tasks.cancel_token().cancel();
        self.tasks.cancel_token().cancel();
    }
}
