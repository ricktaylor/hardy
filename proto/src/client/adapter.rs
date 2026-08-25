// The client's two transfer adapters, one per direction, bridging the
// wire's chunked-transfer grammar and the BPA's [`Segment`] stream:
//
// - [`Reader`] adapts an incoming gRPC stream into a
//   [`Receiver<Segment>`], so a component consumes a transfer it
//   collects (a Receive) or forwards through the same pull interface as
//   any other stream.
// - [`Writer`] adapts the other direction: it pulls a borrowed
//   [`Segment`] stream and pushes wire chunks onto a transfer it sends.
//
// The server's counterpart is `server::adapter::Reader`; the write half
// there is folded into its RPC engines rather than a matching `Writer`.

use core::{future::Future, pin::Pin};

use hardy_bpa::{
    async_trait,
    stream::{Receiver, RecvError, Segment},
};
use tokio::sync::mpsc::Sender;
use tonic::Streaming;

use crate::stream::{Cancel, Chunk, chunks};

// -------------------------------------------------------------------
// Reader: wire -> Segment
// -------------------------------------------------------------------

// The open halves of one transfer: the running RPC's response stream
// and its request sender, the latter kept so an unfinished transfer can
// be abandoned with the wire's in-band cancel.
type Wire<Response, Request> = (Streaming<Response>, Sender<Request>);

// Issues the RPC on the first pull, yielding the transfer's halves or
// `None` if the open fails. Boxed so a [`Reader`] can hold it before the
// first pull without naming the future.
type Opener<Response, Request> = Box<
    dyn FnOnce() -> Pin<Box<dyn Future<Output = Option<Wire<Response, Request>>> + Send>> + Send,
>;

// The `Open` variant is the common, hot state (an eager transfer starts
// there, and a lazy one transits to it on the first pull); `Pending` and
// `Closed` are transient, so boxing the large variant to even the sizes
// would only add an allocation to the path that matters.
#[allow(clippy::large_enum_variant)]
enum State<Response, Request> {
    // Not yet opened: the RPC is issued on the first pull, so a
    // component that never pulls never opens the transfer.
    Pending(Opener<Response, Request>),
    Open {
        chunks: Streaming<Response>,
        requests: Sender<Request>,
    },
    // The open failed, or the transfer ended.
    Closed,
}

// Adapts an incoming gRPC stream into a [`Receiver<Segment>`]: each
// `recv` pulls one wire chunk and hands it over as a segment (HTTP/2
// flow control stalls the BPA beyond its window if the consumer pauses),
// and the wire's last chunk is the final segment. A withdrawal, a
// failure, or a stream ending without the last chunk ends it as
// truncation, never completion; dropping an unfinished collection
// abandons it with the wire's in-band cancel, and the bundle stays held
// for a later attempt.
pub struct Reader<Response, Request: Cancel> {
    state: State<Response, Request>,
    // Set once the last chunk arrives, so dropping a completed
    // collection does not send a pointless cancel.
    completed: bool,
}

impl<Response, Request: Cancel> Reader<Response, Request> {
    // An already-open transfer (a Forward or Dispatch execution): the
    // RPC is running before the first pull.
    pub fn new(chunks: Streaming<Response>, requests: Sender<Request>) -> Self {
        Self {
            state: State::Open { chunks, requests },
            completed: false,
        }
    }

    // A lazily-opened collection: `open` issues the RPC on the first
    // pull, so a component that holds an announced delivery without
    // pulling never opens it and the bundle stays parked for a later
    // registration.
    pub fn lazy<F, Fut>(open: F) -> Self
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Option<Wire<Response, Request>>> + Send + 'static,
    {
        let opener: Opener<Response, Request> = Box::new(move || Box::pin(open()));
        Self {
            state: State::Pending(opener),
            completed: false,
        }
    }
}

#[async_trait]
impl<Response, Request> Receiver<Segment> for Reader<Response, Request>
where
    Response: Chunk + Send + 'static,
    Request: Cancel + Send + 'static,
{
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        // Open on the first pull, at most once.
        if matches!(self.state, State::Pending(_)) {
            let State::Pending(open) = core::mem::replace(&mut self.state, State::Closed) else {
                unreachable!()
            };
            match open().await {
                Some((chunks, requests)) => self.state = State::Open { chunks, requests },
                None => return Err(RecvError),
            }
        }

        let State::Open { chunks, .. } = &mut self.state else {
            return Err(RecvError);
        };
        match chunks.message().await {
            Ok(Some(message)) => match message.into_chunk() {
                Some(segment) => {
                    if matches!(segment, Segment::Final(_)) {
                        self.completed = true;
                    }
                    Ok(segment)
                }
                None => Err(RecvError),
            },
            Ok(None) | Err(_) => Err(RecvError),
        }
    }
}

impl<Response, Request: Cancel> Drop for Reader<Response, Request> {
    // Unlike the session, a Receive half-close is not an ending (a
    // client may send only the metadata), so abandonment needs the
    // explicit in-band cancel. A collection never opened has no request
    // side to cancel.
    fn drop(&mut self) {
        if !self.completed
            && let State::Open { requests, .. } = &self.state
        {
            let _ = requests.try_send(Request::cancel());
        }
    }
}

// -------------------------------------------------------------------
// Writer: Segment -> wire
// -------------------------------------------------------------------

// Adapts a borrowed [`Segment`] stream onto a transfer's request side:
// [`write_all`](Self::write_all) pulls segments and pushes wire chunks
// while the call runs. The write counterpart of [`Reader`], used by the
// sinks that send a transfer to the BPA.
pub struct Writer<'a, Request> {
    requests: &'a Sender<Request>,
}

impl<'a, Request: Chunk + Cancel> Writer<'a, Request> {
    pub fn new(requests: &'a Sender<Request>) -> Self {
        Self { requests }
    }

    // Drains `stream` onto the wire, one chunk per send. A producer that
    // gives up before its final segment aborts in-band, so the BPA
    // discards the partial transfer; a closed request channel (the call
    // already ended) just stops the pump.
    pub async fn write_all(self, stream: &mut dyn Receiver<Segment>) {
        loop {
            let segment = match stream.recv().await {
                Ok(segment) => segment,
                Err(_) => {
                    let _ = self.requests.send(Request::cancel()).await;
                    return;
                }
            };
            let last = matches!(segment, Segment::Final(_));

            for segment in chunks(segment) {
                if self.requests.send(Request::chunk(segment)).await.is_err() {
                    return;
                }
            }
            if last {
                return;
            }
        }
    }
}
