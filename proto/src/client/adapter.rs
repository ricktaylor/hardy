// Adapters between the wire's chunk messages and the BPA's `Segment`
// stream.

use hardy_bpa::{
    async_trait,
    stream::{Receiver, RecvError, Segment},
};
use tokio::sync::mpsc::Sender;
use tonic::Streaming;
use tracing::debug;

use crate::{
    grammar::{Cancel, Chunk},
    transfer::Writer,
};

// Yields one wire chunk per `recv` as a `Segment`. A non-chunk
// message, end of stream, or a transport error surfaces as
// `RecvError`; before a `Final` segment that means truncation.
pub struct ResponseReader<'a, Response> {
    responses: &'a mut Streaming<Response>,
}

impl<'a, Response> ResponseReader<'a, Response> {
    pub fn new(responses: &'a mut Streaming<Response>) -> Self {
        Self { responses }
    }
}

#[async_trait]
impl<Response: Chunk + Send + 'static> Receiver<Segment> for ResponseReader<'_, Response> {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        match self.responses.message().await {
            Ok(Some(message)) => match message.into_chunk() {
                Some(segment) => Ok(segment),
                None => Err(RecvError),
            },
            Ok(None) => Err(RecvError),
            Err(status) => {
                debug!("Transfer stream failed: {status}");
                Err(RecvError)
            }
        }
    }
}

// Feeds `stream` into the transfer's request channel, one chunk per
// segment. If the producer fails before its final segment, a `Cancel`
// message is sent in band.
pub struct RequestWriter<'a, Request> {
    requests_tx: &'a Sender<Request>,
    stream: &'a mut dyn Receiver<Segment>,
}

impl<'a, Request> RequestWriter<'a, Request> {
    pub fn new(requests_tx: &'a Sender<Request>, stream: &'a mut dyn Receiver<Segment>) -> Self {
        Self {
            requests_tx,
            stream,
        }
    }
}

impl<Request: Chunk + Cancel> Writer for RequestWriter<'_, Request> {
    type Error = ();

    async fn next(&mut self) -> Result<Segment, ()> {
        match self.stream.recv().await {
            Ok(segment) => Ok(segment),
            Err(_) => {
                let _ = self.requests_tx.send(Request::cancel()).await;
                Err(())
            }
        }
    }

    async fn write(&mut self, chunk: Segment) -> Result<(), ()> {
        self.requests_tx
            .send(Request::chunk(chunk))
            .await
            .map_err(|_| ())
    }
}
