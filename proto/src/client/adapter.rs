// The client's two transfer adapters, bridging the wire's chunked
// grammar and the BPA's [`Segment`] stream. The server's counterparts
// are `server::adapter::{RequestReader, ResponseWriter}`.

use core::ops::ControlFlow;

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

// One wire chunk per `recv`, as a segment; HTTP/2 flow control stalls
// the BPA beyond its window if the consumer pauses. Anything but the
// wire's last chunk ends the stream as truncation.
//
// Reading is all it does, and the borrow says so: the caller opens the
// call, ends it, and sends whatever the transfer owes the wire (an ack,
// an in-band cancel) on the request side, which the reader never holds.
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

// Writes `stream` onto the transfer's request side, one chunk per send,
// paced by the channel. A producer that gives up before its final
// segment cancels in band, so the BPA discards the partial transfer; a
// closed request channel means the call has already ended.
pub struct RequestWriter<'a, Request> {
    pub requests_tx: &'a Sender<Request>,
    pub stream: &'a mut dyn Receiver<Segment>,
}

impl<Request: Chunk + Cancel> Writer for RequestWriter<'_, Request> {
    type Break = ();

    async fn next(&mut self) -> ControlFlow<(), Segment> {
        match self.stream.recv().await {
            Ok(segment) => ControlFlow::Continue(segment),
            Err(_) => {
                let _ = self.requests_tx.send(Request::cancel()).await;
                ControlFlow::Break(())
            }
        }
    }

    async fn write(&mut self, chunk: Segment) -> ControlFlow<()> {
        match self.requests_tx.send(Request::chunk(chunk)).await {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(()),
        }
    }
}
