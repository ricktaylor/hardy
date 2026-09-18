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
cancel). Each trait is followed by the macro that implements it: the
grammar is spoken by ten generated message types across the four
surfaces, every impl identical modulo the message type, its oneof
field, and the oneof's path, so the macros keep each surface module a
declaration list instead of ninety lines of repeated match arms. The
variant names (`Chunk`/`LastChunk`, the cancel variant) are fixed by
the schemas.

The outgoing sequencing of a transfer (one segment stream, re-framed
as wire chunks) is [`transfer`](crate::transfer)'s, not this
module's: the grammar says what a message can carry, the transfer
drives a whole run of them.
*/

use hardy_bpa::stream::Segment;

/// A message that can carry one segment of bundle bytes; the
/// [`Segment::Final`] ends the transfer.
pub trait Chunk: Sized {
    /// The message carrying `segment`.
    fn chunk(segment: Segment) -> Self;

    /// The carried segment, or `None` for anything else the oneof can
    /// say (metadata, a result, a withdrawal, an empty message).
    fn into_chunk(self) -> Option<Segment>;
}

macro_rules! impl_chunk {
    ($msg:ty, $field:ident, $oneof:ty) => {
        impl $crate::grammar::Chunk for $msg {
            fn chunk(segment: hardy_bpa::stream::Segment) -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(match segment {
                        hardy_bpa::stream::Segment::Next(bytes) => Oneof::Chunk(bytes),
                        hardy_bpa::stream::Segment::Final(bytes) => Oneof::LastChunk(bytes),
                    }),
                }
            }

            fn into_chunk(self) -> Option<hardy_bpa::stream::Segment> {
                type Oneof = $oneof;
                match self.$field {
                    Some(Oneof::Chunk(bytes)) => Some(hardy_bpa::stream::Segment::Next(bytes)),
                    Some(Oneof::LastChunk(bytes)) => Some(hardy_bpa::stream::Segment::Final(bytes)),
                    _ => None,
                }
            }
        }
    };
}

/// A message that can abandon or withdraw a transfer in-band.
pub trait Cancel: Sized {
    /// The message that abandons the transfer.
    fn cancel() -> Self;

    /// Whether this is that message.
    fn is_cancel(&self) -> bool;
}

// `$cancel` is the variant name each schema picks to read naturally in
// its direction (`Cancel` on requests, `Cancelled` on responses).
macro_rules! impl_cancel {
    ($msg:ty, $field:ident, $oneof:ty, $cancel:ident) => {
        impl $crate::grammar::Cancel for $msg {
            fn cancel() -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(Oneof::$cancel(())),
                }
            }

            fn is_cancel(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::$cancel(_)))
            }
        }
    };
}

/// A message that can acknowledge a completed collection in-band,
/// committing it: the delivery is finalized on this, and parked without
/// it.
pub trait Ack: Sized {
    /// The message that commits the collection.
    fn ack() -> Self;

    /// Whether this is that message.
    fn is_ack(&self) -> bool;
}

macro_rules! impl_ack {
    ($msg:ty, $field:ident, $oneof:ty, $ack:ident) => {
        impl $crate::grammar::Ack for $msg {
            fn ack() -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(Oneof::$ack(())),
                }
            }

            fn is_ack(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::$ack(_)))
            }
        }
    };
}

pub(crate) use {impl_ack, impl_cancel, impl_chunk};
