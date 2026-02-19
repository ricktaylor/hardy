use super::*;
use std::{
    net::SocketAddr,
    pin::Pin,
    task::{Context, Poll},
};
use tokio::net::{TcpListener, TcpStream};
use tower::{Service, ServiceExt};

struct ListenerService {
    listener: TcpListener,
    ready: Option<(TcpStream, SocketAddr)>,
}

impl ListenerService {
    fn new(listener: TcpListener) -> Self {
        Self {
            listener,
            ready: None,
        }
    }
}

impl tower::Service<()> for ListenerService {
    type Response = (TcpStream, SocketAddr);
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.listener.poll_accept(cx).map_ok(|(s, a)| {
            self.ready = Some((s, a));
        })
    }

    fn call(&mut self, _: ()) -> Self::Future {
        let (s, a) = self.ready.take().trace_expect("poll_ready not called");
        Box::pin(async move { Ok((s, a)) })
    }
}

pub struct Listener {
    pub connection_rate_limit: u32,
    pub ctx: context::ConnectionContext,
}

impl std::fmt::Debug for Listener {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listener")
            .field("connection_rate_limit", &self.connection_rate_limit)
            .field("ctx", &self.ctx)
            .finish_non_exhaustive()
    }
}

impl Listener {
    #[cfg_attr(feature = "tracing", instrument(skip(tasks)))]
    pub async fn listen(self, tasks: Arc<hardy_async::TaskPool>, address: std::net::SocketAddr) {
        let Ok(listener) = TcpListener::bind(address)
            .await
            .inspect_err(|e| error!("Failed to bind TCP listener: {e:?}"))
        else {
            return;
        };

        // We can layer services here
        let mut svc = tower::ServiceBuilder::new()
            .rate_limit(
                self.connection_rate_limit as u64,
                std::time::Duration::from_secs(1),
            )
            .service(ListenerService::new(listener));

        info!("TCP server listening on {address}");

        loop {
            tokio::select! {
                // Wait for the service to be ready
                r = svc.ready() => match r {
                    Ok(_) => {
                        // Accept a new connection
                        match svc.call(()).await {
                            Ok((stream,remote_addr)) => {
                                info!("New TCP connection from {remote_addr}");
                                // Spawn immediately to prevent head-of-line blocking
                                let ctx = self.ctx.clone();
                                hardy_async::spawn!(tasks, "passive_session_task", async move {
                                    ctx.new_contact(stream, remote_addr).await
                                });
                            }
                            Err(e) => warn!("Failed to accept connection: {e}")
                        }
                    }
                    Err(e) => {
                        warn!("Listener closed: {e}");
                        break;
                    }
                },
                _ = tasks.cancel_token().cancelled() => {
                    break;
                }
            }
        }
    }
}
