use super::*;
use std::{
    pin::Pin,
    task::{Context, Poll},
};
use thiserror::Error;
use tokio::sync::mpsc::*;

#[derive(Error, Debug)]
pub enum Error {
    #[error(transparent)]
    Codec(#[from] codec::Error),

    #[error(transparent)]
    Tower(#[from] tokio_tower::Error<Transport<Vec<u8>, tonic::Status>, Vec<u8>>),
}

#[derive(Error, Debug)]
pub enum TransportError {
    #[error("Channel closed")]
    Closed,
}

pub struct Transport<Request, Response> {
    rcv: UnboundedReceiver<Response>,
    snd: UnboundedSender<Request>,
}

impl<Request, Response> futures::Sink<Request> for Transport<Request, Response> {
    type Error = TransportError;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: Request) -> Result<(), Self::Error> {
        self.snd.send(item).map_err(|_| Self::Error::Closed)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(())) // no-op because all sends succeed immediately
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(())) // no-op because channel is closed on drop and flush is no-op
    }
}

impl<Request, Response> futures::stream::Stream for Transport<Request, Response> {
    type Item = Result<Response, session::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        self.rcv.poll_recv(cx).map(|s| s.map(Ok))
    }
}

pub type Client = dyn tower::Service<
    Vec<u8>,
    Response = tonic::Status,
    Error = Error,
    Future = Pin<Box<dyn futures::Future<Output = Result<tonic::Status, Error>> + Send>>,
>;

pub fn new_client(
    snd: UnboundedSender<Vec<u8>>,
    rcv: UnboundedReceiver<tonic::Status>,
) -> std::sync::Arc<Client> {
    std::sync::Arc::new(tokio_tower::pipeline::Client::with_error_handler(
        Transport { snd, rcv },
        |e: Error| {
            error!("Transport error: {e}");
        },
    ))
}
