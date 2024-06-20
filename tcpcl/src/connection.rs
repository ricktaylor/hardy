use super::*;
use hardy_proto::cla::*;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::sync::mpsc::*;

pub type TowerError =
    tokio_tower::Error<Transport<Vec<u8>, Result<ForwardBundleResponse, tonic::Status>>, Vec<u8>>;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Codec(#[from] codec::Error),

    #[error(transparent)]
    Tower(#[from] TowerError),
}

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("Channel closed")]
    Closed,
}

pub struct Transport<Request, Response>
where
    Request: Send,
{
    rcv: UnboundedReceiver<Response>,
    snd: tokio_util::sync::PollSender<Request>,
}

impl<Request, Response> Transport<Request, Response>
where
    Request: Send,
{
    fn new(snd: Sender<Request>, rcv: UnboundedReceiver<Response>) -> Self {
        Self {
            rcv,
            snd: tokio_util::sync::PollSender::new(snd),
        }
    }
}

impl<Request, Response> futures::Sink<Request> for Transport<Request, Response>
where
    Request: Send,
{
    type Error = TransportError;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        self.snd.poll_reserve(cx).map_err(|_| Self::Error::Closed)
    }

    fn start_send(mut self: Pin<&mut Self>, item: Request) -> Result<(), Self::Error> {
        self.snd.send_item(item).map_err(|_| Self::Error::Closed)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(())) // no-op because all sends succeed immediately
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(())) // no-op because channel is closed on drop and flush is no-op
    }
}

impl<Request, Response> futures::stream::Stream for Transport<Request, Response>
where
    Request: Send,
{
    type Item = Result<Response, session::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.rcv.poll_recv(cx).map(|s| s.map(Ok))
    }
}

pub type Client = Box<
    dyn tower::Service<
            Vec<u8>,
            Response = Result<ForwardBundleResponse, tonic::Status>,
            Error = Error,
            Future = Pin<
                Box<
                    dyn futures::Future<
                            Output = Result<Result<ForwardBundleResponse, tonic::Status>, Error>,
                        > + Send,
                >,
            >,
        > + Send,
>;

pub fn new_client(
    snd: Sender<Vec<u8>>,
    rcv: UnboundedReceiver<Result<ForwardBundleResponse, tonic::Status>>,
) -> Client {
    Box::new(tokio_tower::pipeline::Client::with_error_handler(
        Transport::new(snd, rcv),
        |e: Error| {
            error!("Transport error: {e}");
        },
    ))
}
