// The Receive-door half of a client session, shared across the
// surfaces that deliver bundles (applications, low-level services). It
// is not an adapter: it mints one collection ([`adapter::Reader`]) per
// announced delivery, on demand. The generated service clients are
// concrete, so each surface supplies its Receive capability through
// [`ReceiveDoor`] and shares this generic collector.

use hardy_bpa::{Bytes, async_trait};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::Streaming;

use super::adapter;
use crate::stream::{Cancel, Chunk};

// A surface's Receive door: how to build the metadata that opens a
// collection, and how to open the RPC itself. Implemented on each
// surface's generated client.
#[async_trait]
pub trait ReceiveDoor: Clone + Send + Sync + 'static {
    // The Receive request message (carries the cancel signal).
    type Request: Cancel + Send + 'static;
    // The Receive response message (carries the wire chunks).
    type Response: Chunk + Send + 'static;

    // Builds the Receive metadata, the first message of the RPC.
    fn metadata(token: &Bytes, bundle_id: String) -> Self::Request;

    // Opens the Receive RPC with `requests` as its request stream,
    // yielding the response stream, or `None` if the call fails.
    async fn open(
        &self,
        requests: ReceiverStream<Self::Request>,
    ) -> Option<Streaming<Self::Response>>;
}

// The session's Receive door: the client and its session token, cloned
// into every announced delivery's lazy collection by the event loop.
#[derive(Clone)]
pub struct Collector<D: ReceiveDoor> {
    door: D,
    token: Bytes,
}

impl<D: ReceiveDoor> Collector<D> {
    pub fn new(door: D, token: Bytes) -> Self {
        Self { door, token }
    }

    // Opens the announced delivery `bundle_id` for collection, lazily:
    // the Receive RPC is issued only on the first pull, so a component
    // that holds the announcement without pulling never opens it and the
    // bundle stays parked for a later registration. The request side
    // stays open so the collection can be abandoned with the wire's
    // in-band cancel.
    pub fn open(&self, bundle_id: String) -> adapter::Reader<D::Response, D::Request> {
        let door = self.door.clone();
        let token = self.token.clone();
        adapter::Reader::lazy(move || async move {
            let (requests, rx) = mpsc::channel(2);
            requests.send(D::metadata(&token, bundle_id)).await.ok()?;
            let chunks = door.open(ReceiverStream::new(rx)).await?;
            Some((chunks, requests))
        })
    }
}
