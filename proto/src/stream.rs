/*!
The chunked-transfer grammar every data-plane stream speaks, in both
directions on both ends: bundle bytes travel as a run of `chunk`
messages ended by `last_chunk` (possibly the only one, possibly
empty); a stream ending without it was truncated, and commits
nothing; an in-band `cancel` abandons or withdraws it. What commits a
completed transfer depends on its direction: a transfer towards the
BPA (a Send, a Dispatch) commits on `last_chunk` itself, while a
collection (a Receive) commits only on the client's in-band `ack`,
sent after `last_chunk`.

The traits are capabilities of the generated message types (a
[`SendRequest`](crate::service::SendRequest) can carry a chunk and a
cancel, a [`ReceiveRequest`](crate::service::ReceiveRequest) only a
cancel), implemented alongside the other wire conversions in the crate
root. The engines that pump them — paced, cancellable — live with
their side, in `client` and `server`.
*/

use hardy_bpa::stream::Segment;

use crate::CHUNK_SIZE;

/// A message that can carry one segment of bundle bytes; the
/// [`Segment::Final`] ends the transfer.
pub trait Chunk: Sized {
    fn chunk(segment: Segment) -> Self;
    /// The carried segment, or `None` for anything else the oneof can
    /// say (metadata, a result, a withdrawal, an empty message).
    fn into_chunk(self) -> Option<Segment>;
}

/// A message that can abandon or withdraw a transfer in-band.
pub trait Cancel: Sized {
    fn cancel() -> Self;
    fn is_cancel(&self) -> bool;
}

/// A message that can acknowledge a completed collection in-band,
/// committing it: the delivery is finalized on this, and parked without
/// it.
pub trait Ack: Sized {
    fn ack() -> Self;
    fn is_ack(&self) -> bool;
}

/// A message that can end its Subscribe session gracefully.
pub trait Unregister {
    fn is_unregister(&self) -> bool;
}

/// Re-frames one segment as wire chunks: [`CHUNK_SIZE`]-bounded
/// segments, [`Segment::Next`] until the [`Segment::Final`] that
/// ends the transfer. An empty final segment still yields its
/// `Final` marker; an empty intermediate segment yields nothing.
pub fn chunks(segment: Segment) -> impl Iterator<Item = Segment> {
    let (mut bytes, last) = match segment {
        Segment::Next(bytes) => (bytes, false),
        Segment::Final(bytes) => (bytes, true),
    };
    let mut done = bytes.is_empty() && !last;
    core::iter::from_fn(move || {
        if done {
            return None;
        }
        let slice = bytes.split_to(bytes.len().min(CHUNK_SIZE));
        done = bytes.is_empty();
        Some(if last && done {
            Segment::Final(slice)
        } else {
            Segment::Next(slice)
        })
    })
}
