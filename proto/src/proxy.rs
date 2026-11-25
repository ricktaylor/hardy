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

/// Pending response map, `Some` while the reader is alive and `None` once
/// it has exited.
///
/// `call()` checks this before inserting: with no reader left to correlate
/// a response, it returns straight away.
type PendingMap<R> =
    Arc<Mutex<Option<HashMap<u32, tokio::sync::oneshot::Sender<Result<R, tonic::Status>>>>>>;

/// The answer to the message that opened a session, and whether the
/// session carries on behind it.
enum Opening<S> {
    /// The session is open: everything held back follows the answer.
    Accepted(S),
    /// The answer is the session's last word.
    Refused(S),
}

/// A serving end's state until its session opens: the answer it owes the
/// peer, and the inbound stream it has yet to serve.
struct Unopened<S, R> {
    answer: tokio::sync::oneshot::Sender<Opening<S>>,
    stream: tonic::Streaming<R>,
}

/// Writer half: reads from a channel and sends on the gRPC outbound
/// stream.
///
/// This task owns the outbound direction; a clone of `write_tx` is the only
/// way to send on it.
async fn writer_task<S>(
    opening: Option<tokio::sync::oneshot::Receiver<Opening<S>>>,
    write_rx: tokio::sync::mpsc::Receiver<S>,
    tx: tokio::sync::mpsc::Sender<S>,
    cancel: hardy_async::CancellationToken,
) {
    if send_opening(opening, &tx, &cancel).await {
        relay(write_rx, &tx, &cancel).await;
    }

    // The writer owns the outbound direction, so its exit ends the session.
    // Cancelling here is also what lets `fail` return as soon as a refusal
    // is on its way, rather than wait on the peer.
    cancel.cancel();
}

/// Sends the answer a serving end owes the message that opened its
/// session, and reports whether anything follows it.
///
/// Holding the rest of the session behind this answer is what keeps a
/// component the BPA puts to work the moment it is registered from
/// reaching the peer before its registration response does. A connecting
/// end owes no answer, so `None` opens the session outright.
async fn send_opening<S>(
    opening: Option<tokio::sync::oneshot::Receiver<Opening<S>>>,
    tx: &tokio::sync::mpsc::Sender<S>,
    cancel: &hardy_async::CancellationToken,
) -> bool {
    let Some(opening) = opening else {
        return true;
    };

    let opening = tokio::select! {
        opening = opening => opening,
        _ = cancel.cancelled() => return false,
    };

    match opening {
        // The session let the proxy go without answering.
        Err(_) => false,
        // A refusal is the session's last word.
        Ok(Opening::Refused(msg)) => {
            let _ = tx.send(msg).await;
            false
        }
        Ok(Opening::Accepted(msg)) => tx
            .send(msg)
            .await
            .inspect_err(|_| debug!("Outbound channel closed"))
            .is_ok(),
    }
}

/// Forwards what the session produces until it is cancelled or either end
/// of the channel closes.
async fn relay<S>(
    mut write_rx: tokio::sync::mpsc::Receiver<S>,
    tx: &tokio::sync::mpsc::Sender<S>,
    cancel: &hardy_async::CancellationToken,
) {
    loop {
        tokio::select! {
            msg = write_rx.recv() => {
                let Some(msg) = msg else { break };
                if tx.send(msg).await.is_err() {
                    debug!("Outbound channel closed");
                    break;
                }
            }
            _ = cancel.cancelled() => break,
        }
    }
}

/// Reader half: a demultiplexer over the gRPC inbound stream.
///
/// The stream carries both responses to this end's own `call()`s, each of
/// which unblocks a handler, and requests from the peer. The reader must
/// never block on anything a handler depends on, or it cannot deliver the
/// responses that let handlers finish. Requests are therefore spawned and
/// their concurrency permit acquired inside the spawned task, never on the
/// read path, where it would stall the reader against the very responses
/// that release it.
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
        // from `on_close` hits its guard and returns instead of awaiting its
        // own task.
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

        // An id present in the pending map is a response to a call made
        // from this end: the per-side parity (see `Side`) guarantees it
        // cannot be a colliding request from the peer. Bind before matching
        // so the pending lock is released either way.
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

/// Which end of a proxied connection this is.
///
/// The two ends draw request ids from disjoint parities: [`Side::Client`]
/// odd, [`Side::Server`] even, the same convention HTTP/2 uses for stream
/// ids. Because neither end can mint an id the other might, an inbound id
/// found in the pending map is unambiguously a response to a call this end
/// made, never a colliding request the peer has in flight.
#[derive(Clone, Copy)]
pub enum Side {
    Client,
    Server,
}

impl Side {
    // The first id this side hands out; both step by 2 from here, so the
    // client's odd ids and the server's even ids never collide. The message
    // that opens a session carries 0, which neither side mints.
    fn first_id(self) -> u32 {
        match self {
            Side::Client => 1,
            Side::Server => 2,
        }
    }
}

/// The serving end of a session, whose outbound items carry the RPC's
/// terminal status.
impl<T, R> RpcProxy<Result<T, tonic::Status>, R>
where
    R: RecvMsg + Send + 'static,
    R::Msg: Send,
    T: SendMsg + Send + 'static,
    T::Msg: Send,
{
    /// Reads the message that opens a session and builds a proxy for it,
    /// returning that message and the id its answer carries.
    ///
    /// The session is not live yet: nothing is sent and nothing is read
    /// until [`open`](Self::open) or [`fail`](Self::fail) settles the
    /// answer the peer is waiting for.
    pub async fn accept(
        channel_sender: Sender<Result<T, tonic::Status>>,
        mut channel_receiver: tonic::Streaming<R>,
    ) -> Result<(Arc<Self>, u32, R::Msg), tonic::Status> {
        let msg = channel_receiver
            .message()
            .await?
            .ok_or_else(|| tonic::Status::unavailable("Session closed before it opened"))?;

        let msg_id = msg.msg_id();
        let opening = match msg.msg() {
            Ok(opening) => opening,
            Err(status) => {
                // There is no proxy to refuse through yet, so the status
                // goes out on the raw stream.
                let _ = channel_sender.send(Err(status.clone())).await;
                return Err(status);
            }
        };

        let proxy = Self::new(channel_sender, Side::Server, Some(channel_receiver));
        Ok((Arc::new(proxy), msg_id, opening))
    }

    /// Answers the opening message and brings the session up.
    ///
    /// `handler` takes everything the peer sends from here on, and whatever
    /// the session produced in the meantime follows the answer out.
    pub fn open(
        &self,
        msg_id: u32,
        msg: T::Msg,
        handler: Box<dyn ProxyHandler<SMsg = T::Msg, RMsg = R::Msg>>,
    ) {
        // Bind before matching, so the lock is released whichever way it goes.
        let unopened = self.unopened.lock().take();
        if let Some(Unopened { answer, stream }) = unopened {
            let _ = answer.send(Opening::Accepted(SendMsg::compose(msg_id, msg)));
            self.read(stream, handler);
        }
    }

    /// Refuses the opening message, making `status` the RPC's terminal
    /// status so the session never comes up.
    ///
    /// Returns once the refusal is on its way, so a caller that drops the
    /// proxy here does not take the refusal down with it.
    pub async fn fail(&self, status: tonic::Status) {
        let unopened = self.unopened.lock().take();
        let Some(Unopened { answer, .. }) = unopened else {
            return;
        };

        let _ = answer.send(Opening::Refused(Err(status)));
        self.tasks.cancel_token().cancelled().await;
    }
}

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
    /// The answer a serving end owes its peer, until it opens or refuses
    /// the session. `None` throughout for a connecting end.
    unopened: Mutex<Option<Unopened<S, R>>>,
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
    /// Sends a message and awaits its response, for the handshake phase
    /// before `run()`, while the caller still owns both halves directly.
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

    /// Starts the proxy.
    ///
    /// `side` selects the request-id parity for this end (see [`Side`]); the
    /// two ends of a connection must pass opposite sides. The reader, the
    /// writer and the per-request handlers all run on one task pool, which
    /// [`shutdown`](Self::shutdown) cancels and drains and dropping the
    /// proxy cancels.
    ///
    /// From here, [`call`](Self::call) sends messages and awaits their
    /// responses. A serving end, which owes its peer an answer before
    /// anything else, starts with [`accept`](Self::accept) instead.
    pub fn run(
        channel_sender: Sender<S>,
        channel_receiver: tonic::Streaming<R>,
        handler: Box<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
        side: Side,
    ) -> Self {
        let proxy = Self::new(channel_sender, side, None);
        proxy.read(channel_receiver, handler);
        proxy
    }

    /// Spawns the writer, which is all a session has before it opens.
    fn new(channel_sender: Sender<S>, side: Side, unopened: Option<tonic::Streaming<R>>) -> Self {
        let tasks = hardy_async::TaskPool::new();
        let (write_tx, write_rx) = tokio::sync::mpsc::channel(16);
        let pending: PendingMap<R::Msg> = Arc::new(Mutex::new(Some(HashMap::new())));

        // A serving end owes the peer an answer to the message that opened
        // the session. Until it settles, the writer holds every other
        // message back and the inbound stream stays unread, so the peer
        // reads the answer first and no work arrives before there is a
        // handler to take it.
        let (unopened, opening) = match unopened {
            Some(stream) => {
                let (answer, opening) = tokio::sync::oneshot::channel();
                (Some(Unopened { answer, stream }), Some(opening))
            }
            None => (None, None),
        };

        let writer_cancel = tasks.cancel_token().clone();
        hardy_async::spawn!(tasks, "rpc_proxy_writer", async move {
            writer_task(opening, write_rx, channel_sender, writer_cancel).await;
        });

        Self {
            write_tx,
            pending,
            next_msg_id: AtomicU32::new(side.first_id()),
            unopened: Mutex::new(unopened),
            tasks,
        }
    }

    /// Spawns the reader, which serves the peer through `handler`.
    fn read(
        &self,
        channel_receiver: tonic::Streaming<R>,
        handler: Box<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
    ) {
        let reader = Reader::new(
            channel_receiver,
            self.write_tx.clone(),
            self.pending.clone(),
            Arc::from(handler),
            self.tasks.clone(),
        );
        hardy_async::spawn!(
            self.tasks,
            "rpc_proxy_reader",
            async move { reader.run().await }
        );
    }

    /// Sends a message and awaits the correlated response.
    ///
    /// The message goes out through the writer channel, so the reader task
    /// is never blocked. A oneshot keyed by the message id is registered in
    /// the pending map, and the reader completes it on the matching
    /// response.
    pub async fn call(&self, msg: S::Msg) -> Result<Option<R::Msg>, tonic::Status> {
        let msg_id = self.next_msg_id.fetch_add(2, Ordering::Relaxed);

        let (ret_tx, ret_rx) = tokio::sync::oneshot::channel();

        // Register the pending response before sending, and give up at
        // once if the reader has exited, leaving nothing to correlate the
        // response.
        {
            let mut guard = self.pending.lock();
            let Some(map) = guard.as_mut() else {
                return Ok(None);
            };
            map.insert(msg_id, ret_tx);
        }

        if self.write_tx.send(S::compose(msg_id, msg)).await.is_err() {
            if let Some(map) = self.pending.lock().as_mut() {
                map.remove(&msg_id);
            }
            return Ok(None);
        }

        let Ok(r) = ret_rx.await else {
            return Ok(None);
        };
        r.map(Some)
    }

    /// Cancels all tasks and awaits their completion.
    ///
    /// Idempotent. The reader and the writer both cancel the pool on exit,
    /// so once the connection is gone the guard below short-circuits any
    /// re-entrant call from `on_close`: it returns instead of awaiting its
    /// own task. A `shutdown` from within a still-running handler would
    /// still self-deadlock; drop the proxy there instead.
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
        // Cancel the tasks so the stream closes promptly: an abandoned
        // proxy leaves no orphaned tasks on the runtime.
        self.tasks.cancel_token().cancel();
    }
}
