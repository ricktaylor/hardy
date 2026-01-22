use super::*;
use hardy_bpa::async_trait;
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use tracing::{debug, error, info, warn};

mod cla;
mod service;

pub use cla::register_cla;
pub use service::register_service;

pub trait SendMsg {
    type Msg;

    fn compose(msg_id: u32, msg: Self::Msg) -> Self;
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

pub trait RecvMsg {
    type Msg;

    fn msg_id(&self) -> u32;
    fn msg(self) -> Result<Self::Msg, tonic::Status>;
}

#[async_trait]
pub trait ProxyHandler: Send + Sync {
    type SMsg;
    type RMsg;

    async fn on_notify(&self, msg: Self::RMsg) -> Option<Self::SMsg>;
    async fn on_close(&self);
}

struct Msg<S, R> {
    msg: S,
    ret: tokio::sync::oneshot::Sender<Result<R, tonic::Status>>,
}

type Receiver<S, R> = tokio::sync::mpsc::Receiver<Option<Msg<S, R>>>;
type Sender<S> = tokio::sync::mpsc::Sender<S>;

async fn notify<S, RMsg>(
    tx: &Sender<S>,
    msg_id: u32,
    msg: RMsg,
    handler: &dyn ProxyHandler<SMsg = S::Msg, RMsg = RMsg>,
) where
    S: SendMsg,
{
    let msg = if let Some(msg) = handler.on_notify(msg).await {
        msg
    } else {
        return;
    };

    _ = tx
        .send(S::compose(msg_id, msg))
        .await
        .inspect_err(|e| error!("Failed to send response: {e}"))
}

async fn run<S, R>(
    mut stream: tonic::Streaming<R>,
    mut rx: Receiver<S::Msg, R::Msg>,
    tx: Sender<S>,
    handler: Box<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
) where
    R: RecvMsg,
    S: SendMsg,
{
    let mut msg_id = 1u32;
    let mut pending_acks: HashMap<
        u32,
        tokio::sync::oneshot::Sender<Result<R::Msg, tonic::Status>>,
    > = HashMap::new();

    loop {
        tokio::select! {
            msg = stream.message() => {
                match msg {
                    Err(e) => {
                        error!("gRPC connection failed: {e}");
                        break;
                    }
                    Ok(None) => {
                        // Client has ended
                        debug!("gRPC connection closed");
                        break;
                    }
                    Ok(Some(msg)) => {
                        let msg_id = msg.msg_id();
                        if let Some(ret) = pending_acks.remove(&msg_id) {
                            _ = ret.send(msg.msg());
                        } else {
                            match msg.msg() {
                                Ok(msg) => notify(&tx,msg_id,msg,handler.as_ref()).await,
                                Err(status) => warn!("{status}"),
                            }
                        }
                    }
                }
            }
            msg = rx.recv() => {
                match msg {
                    None | Some(None) => {
                        // Sink is closing
                        debug!("Proxy closed");
                        break;
                    }
                    Some(Some(msg)) => {
                        msg_id = msg_id.wrapping_add(1);
                        pending_acks.insert(msg_id,msg.ret);

                        if tx.send(S::compose(msg_id, msg.msg))
                            .await.is_err()
                            && let Some(ret) = pending_acks.remove(&msg_id) {
                            _ = ret.send(Err(tonic::Status::cancelled("Closed")));
                        }
                    }
                }
            }
        }
    }

    handler.on_close().await;
}

#[allow(clippy::type_complexity)]
pub struct RpcProxy<S, R>
where
    R: RecvMsg + Send,
    R::Msg: Send,
    S: SendMsg + Send,
    S::Msg: Send,
{
    tx: tokio::sync::mpsc::Sender<Option<Msg<S::Msg, R::Msg>>>,
    tasks: hardy_async::task_pool::TaskPool,
}

impl<S, R> RpcProxy<S, R>
where
    R: RecvMsg + Send + 'static,
    R::Msg: Send,
    S: SendMsg + Send + 'static,
    S::Msg: Send,
{
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

    pub fn run(
        channel_sender: Sender<S>,
        channel_receiver: tonic::Streaming<R>,
        handler: Box<dyn ProxyHandler<SMsg = S::Msg, RMsg = R::Msg>>,
    ) -> Self {
        // Now create the worker
        let (tx, rx) = tokio::sync::mpsc::channel(16);

        let tasks = hardy_async::task_pool::TaskPool::new();
        hardy_async::spawn!(tasks, "rpc_proxy_run", async move {
            run(channel_receiver, rx, channel_sender, handler).await;
        });

        Self { tx, tasks }
    }

    pub async fn call(&self, msg: S::Msg) -> Result<Option<R::Msg>, tonic::Status> {
        let (ret, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Some(Msg { msg, ret })).await.is_err() {
            return Ok(None);
        };
        let Ok(r) = rx.await else {
            return Ok(None);
        };
        r.map(Some)
    }

    pub async fn close(&self) {
        // Send hangup message
        _ = self.tx.send(None).await;

        // Wait for run() to exit
        self.tasks.shutdown().await;
    }
}
