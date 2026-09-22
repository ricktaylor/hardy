/*!
The client SDK: registers local components (applications, services,
convergence-layer adapters, and routing agents) against a remote BPA
over the gRPC v1 wire, using the same traits a local
[`Bpa`](hardy_bpa::bpa::Bpa) takes.
*/

use core::{future::Future, num::NonZeroUsize, pin::pin};

use hardy_bpa::stream::{Receiver, Segment};
use tokio::{select, sync::mpsc};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Response, Status};

use self::adapter::RequestWriter;
use crate::{
    grammar::{Cancel, Chunk},
    transfer::Writer,
};

mod adapter;
mod bpa_client;
mod services;

pub use self::bpa_client::{BpaClient, EndpointError, RegistrationHandle};
/// The tonic transport endpoint, re-exported for
/// [`BpaClient::with_endpoint`] callers.
pub use tonic::transport::Endpoint;

// Kept small so unsent chunks backpressure the transfer's producer.
pub(crate) const TRANSFER_REQUEST_CAPACITY: usize = 2;

// Holds a Register, a later Unregister, and headroom.
pub(crate) const SUBSCRIBE_REQUEST_CAPACITY: usize = 4;

// Cap on deliveries one registration runs concurrently. When full,
// the event loop stops reading the session stream.
pub(crate) const MAX_CONCURRENT_DELIVERIES: NonZeroUsize = NonZeroUsize::new(4).unwrap();

// Runs one client-streaming call: sends `metadata`, then `stream` as
// chunks, and returns the response. The server may respond before the
// transfer completes.
async fn write_transfer<Req, Rsp, Call, Fut>(
    metadata: Req,
    stream: &mut dyn Receiver<Segment>,
    call: Call,
) -> Result<Response<Rsp>, Status>
where
    Req: Chunk + Cancel + Send + 'static,
    Call: FnOnce(ReceiverStream<Req>) -> Fut,
    Fut: Future<Output = Result<Response<Rsp>, Status>>,
{
    let (requests_tx, requests_rx) = mpsc::channel::<Req>(TRANSFER_REQUEST_CAPACITY);
    let writing = async move {
        if requests_tx.send(metadata).await.is_err() {
            return;
        }
        let _ = RequestWriter::new(&requests_tx, stream).write_all().await;
    };

    let mut call = pin!(call(ReceiverStream::new(requests_rx)));
    // Biased: an already-ready response wins over more writing.
    let answered = select! {
        biased;
        response = &mut call => Some(response),
        () = writing => None,
    };
    match answered {
        Some(response) => response,
        // The writer finished and dropped its sender, half-closing the
        // request side; wait for the response.
        None => call.await,
    }
}
