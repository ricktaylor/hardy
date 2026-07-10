use core::sync::atomic::{AtomicU32, Ordering};
use std::collections::HashMap;

use hardy_async::sync::spin::Mutex;

use super::*;

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

/// Reader half: a pure demultiplexer over the gRPC inbound stream.
///
/// The stream multiplexes responses to our own `call()`s (which unblock a
/// handler somewhere) with requests from the peer. The reader must never block
/// on anything a handler depends on, or it cannot deliver the responses that
/// let handlers finish. So requests are spawned and their concurrency permit is
/// acquired *inside* the task, never on the read path — acquiring it here would
/// stall the reader against the very responses that release it.
struct Reader<S, R>
where
    R: RecvMsg,
    S: SendMsg,
{
    stream: tonic::Streaming<R>,
    write_tx: tokio::sync::mpsc::Sender<S>,
    pending: PendingMap<R::Msg>,
    handler: Arc<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
    tasks: hardy_async::TaskPool,
    permits: Arc<tokio::sync::Semaphore>,
}

impl<S, R> Reader<S, R>
where
    R: RecvMsg + Send + 'static,
    R::Msg: Send + 'static,
    S: SendMsg + Send + 'static,
    S::Msg: Send + 'static,
{
    fn new(
        stream: tonic::Streaming<R>,
        write_tx: tokio::sync::mpsc::Sender<S>,
        pending: PendingMap<R::Msg>,
        handler: Arc<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
        tasks: hardy_async::TaskPool,
    ) -> Self {
        Self {
            stream,
            write_tx,
            pending,
            handler,
            tasks,
            permits: Arc::new(tokio::sync::Semaphore::new(
                hardy_async::available_parallelism().get(),
            )),
        }
    }

    async fn run(mut self) {
        let cancel = self.tasks.cancel_token().clone();
        loop {
            let msg = tokio::select! {
                r = self.stream.message() => match r {
                    Ok(Some(msg)) => msg,
                    Ok(None) => { debug!("gRPC connection closed"); break; }
                    Err(e) => { error!("gRPC connection failed: {e}"); break; }
                },
                _ = cancel.cancelled() => break,
            };
            self.dispatch(msg);
        }

        // The reader exiting means the connection is gone: wind the whole proxy
        // down. Cancelling here (not only on the external `shutdown` path)
        // makes "reader exited" imply "cancelled", so a re-entrant `shutdown`
        // from `on_close`/`on_unregister` hits its guard and returns instead of
        // awaiting its own task.
        cancel.cancel();

        // Fail in-flight calls (unblocking handlers parked in `call()`) and
        // reject new ones.
        let pending_calls: Vec<_> = self.pending.lock().take().into_iter().flatten().collect();
        for (_, ret) in pending_calls {
            let _ = ret.send(Err(tonic::Status::cancelled("Connection closed")));
        }

        self.handler.on_close().await;
    }

    fn dispatch(&self, msg: R) {
        let msg_id = msg.msg_id();

        // Bind before matching so the pending lock is released either way.
        let pending_sender = self.pending.lock().as_mut().and_then(|m| m.remove(&msg_id));
        if let Some(ret) = pending_sender {
            let _ = ret.send(msg.msg());
            return;
        }

        let req = match msg.msg() {
            Ok(req) => req,
            Err(status) => {
                warn!("{status}");
                return;
            }
        };

        let handler = self.handler.clone();
        let write_tx = self.write_tx.clone();
        let permits = self.permits.clone();
        let cancel = self.tasks.cancel_token().clone();
        hardy_async::spawn!(self.tasks, "rpc_proxy_handler", async move {
            let _permit = tokio::select! {
                permit = permits.acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => return,
                },
                _ = cancel.cancelled() => return,
            };
            if let Some(response) = handler.on_notify(req).await {
                let _ = write_tx
                    .send(S::compose(msg_id, response))
                    .await
                    .inspect_err(|_| debug!("Response dropped (connection closed)"));
            }
        });
    }
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
    next_msg_id: AtomicU32,
    /// Reader, writer, and per-request handlers.
    tasks: hardy_async::TaskPool,
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

    /// Start the proxy.
    ///
    /// The reader, writer, and per-request handlers all run on one task pool.
    /// Call `shutdown()` to cancel and drain them, or drop the proxy to cancel
    /// them.
    ///
    /// After this call, use `call()` to send messages and await responses.
    pub fn run(
        channel_sender: Sender<S>,
        channel_receiver: tonic::Streaming<R>,
        handler: Box<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
    ) -> Self {
        let tasks = hardy_async::TaskPool::new();
        let (write_tx, write_rx) = tokio::sync::mpsc::channel(16);
        let pending: PendingMap<R::Msg> = Arc::new(Mutex::new(Some(HashMap::new())));

        let writer_cancel = tasks.cancel_token().clone();
        hardy_async::spawn!(tasks, "rpc_proxy_writer", async move {
            writer_task(write_rx, channel_sender, writer_cancel).await;
        });

        let reader = Reader::new(
            channel_receiver,
            write_tx.clone(),
            pending.clone(),
            Arc::from(handler),
            tasks.clone(),
        );
        hardy_async::spawn!(tasks, "rpc_proxy_reader", async move { reader.run().await });

        Self {
            write_tx,
            pending,
            next_msg_id: AtomicU32::new(1),
            tasks,
        }
    }

    /// Send a message and await the correlated response.
    ///
    /// The message is sent via the writer channel (non-blocking with respect
    /// to the reader task). A oneshot is registered in the pending map, keyed
    /// by msg_id. The reader task completes the oneshot when it sees the
    /// matching response.
    pub async fn call(&self, msg: S::Msg) -> Result<Option<R::Msg>, tonic::Status> {
        // fetch_add wraps; 0 is reserved for the handshake, so the caller that
        // draws it takes the next id instead.
        let mut msg_id = self.next_msg_id.fetch_add(1, Ordering::Relaxed);
        if msg_id == 0 {
            msg_id = self.next_msg_id.fetch_add(1, Ordering::Relaxed);
        }

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
        self.tasks.cancel_token().cancel();
    }

    /// Cancel all tasks and await their completion.
    ///
    /// Idempotent. The reader cancels the pool on exit, so once the connection
    /// is gone the guard below short-circuits any re-entrant call from
    /// `on_close`/`on_unregister` — it returns instead of awaiting its own
    /// task. A `shutdown` from within a still-running handler would still
    /// self-deadlock; use [`cancel`](Self::cancel) there.
    pub async fn shutdown(&self) {
        if self.tasks.is_cancelled() {
            return;
        }
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
        self.tasks.cancel_token().cancel();
    }
}
