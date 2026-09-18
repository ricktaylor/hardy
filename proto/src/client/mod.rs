/*!
The client SDK: a local component registers against a remote BPA over
the v1 wire with the same traits a local [`Bpa`](hardy_bpa::bpa::Bpa)
uses, and the SDK carries the sessions, tokens, and data-plane calls.

All four surfaces are served: applications, low-level services,
convergence-layer adapters, and routing agents.
*/

use core::{future::Future, num::NonZeroUsize, pin::pin};

use hardy_bpa::stream::{Receiver, Segment};
use tokio::{select, sync::mpsc};
use tokio_stream::wrappers::ReceiverStream;
// `Response` is this module's generic parameter for a wire response
// message, so tonic's own wrapper is aliased.
use tonic::{Response as RpcResponse, Status};

use self::adapter::RequestWriter;
use crate::{
    grammar::{Cancel, Chunk},
    transfer::Writer,
};

mod adapter;
mod bpa_client;
mod services;

pub use self::bpa_client::{BpaClient, EndpointError, RegistrationHandle};
/// Re-exported for [`BpaClient::with_endpoint`] callers, so holding or
/// passing an endpoint needs no direct `tonic` dependency.
pub use tonic::transport::Endpoint;

// The request channel of one data-plane transfer. Small on purpose,
// since it is where a transfer's backpressure comes from; every send on
// it is awaited, so nothing depends on the depth.
pub(crate) const TRANSFER_REQUEST_CAPACITY: usize = 2;

// The request channel of a Subscribe session: the Register handshake plus
// a later Unregister, with headroom.
pub(crate) const SUBSCRIBE_REQUEST_CAPACITY: usize = 4;

// How many announced deliveries one registration collects at once:
// enough that one slow collection does not serialise the rest, small
// enough that a registration cannot monopolise its connection. Beyond
// it, the announcement loop waits for a slot, backpressuring the
// session stream and through it the BPA.
pub(crate) const MAX_CONCURRENT_DELIVERIES: NonZeroUsize = NonZeroUsize::new(4).unwrap();

// Runs one streamed request call to its response, writing `stream` onto
// the request side behind `metadata`, which the wire requires first.
//
// The response ends this, never the writer. A server may answer before
// the transfer is complete, and a writer parked on a stalled producer
// would hold that answer back indefinitely, so the two race and a
// response drops the writer. The ordinary path is the writer finishing
// first: dropping the request sender half-closes the request side, and
// the server answers.
async fn write_transfer<Request, Response, Call, Fut>(
    metadata: Request,
    stream: &mut dyn Receiver<Segment>,
    call: Call,
) -> Result<RpcResponse<Response>, Status>
where
    Request: Chunk + Cancel + Send + 'static,
    Call: FnOnce(ReceiverStream<Request>) -> Fut,
    Fut: Future<Output = Result<RpcResponse<Response>, Status>>,
{
    let (requests_tx, requests_rx) = mpsc::channel::<Request>(TRANSFER_REQUEST_CAPACITY);
    let writing = async move {
        if requests_tx.send(metadata).await.is_err() {
            return;
        }
        // Complete or not, this side is done; the response is what
        // answers the call.
        let _ = RequestWriter::new(&requests_tx, stream).write_all().await;
    };

    let mut call = pin!(call(ReceiverStream::new(requests_rx)));
    // The response is polled first so an answer already waiting is
    // taken over another turn of the writer.
    let answered = select! {
        biased;
        response = &mut call => Some(response),
        () = writing => None,
    };
    match answered {
        Some(response) => response,
        // The writer is dropped with the `select!`, so the request side
        // is closed before the call is awaited to its response.
        None => call.await,
    }
}
