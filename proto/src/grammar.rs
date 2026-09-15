/*!
The chunked-transfer grammar spoken on every data-plane stream. Bundle
bytes travel as a run of `chunk` messages ended by one `last_chunk`. A
stream that ends without `last_chunk` is truncated and commits
nothing; an in-band `cancel` abandons the transfer. A transfer towards
the BPA commits on `last_chunk`; a collection commits on the client's
in-band `ack`.

Each trait names one capability of the generated message types and is
implemented for them by the macro that follows it.
*/

use hardy_bpa::stream::Segment;

/// A message that can carry one [`Segment`] of bundle bytes.
///
/// A [`Segment::Final`] ends the transfer.
pub trait Chunk: Sized {
    /// Creates the message carrying `segment`.
    fn chunk(segment: Segment) -> Self;

    /// Returns the carried segment, or `None` if the message carries
    /// anything else.
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
    /// Creates the cancel message.
    fn cancel() -> Self;

    /// Returns `true` if this is the cancel message.
    fn is_cancel(&self) -> bool;
}

// `$cancel` names the oneof variant: `Cancel` on request messages,
// `Cancelled` on response messages.
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

/// A message that acknowledges a completed collection in-band.
///
/// The delivery commits only when this message arrives.
pub trait Ack: Sized {
    /// Creates the ack message.
    fn ack() -> Self;

    /// Returns `true` if this is the ack message.
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

/// A session request that can end its own session in-band.
pub trait Unregister {
    /// Returns `true` if this is the message that ends the session.
    fn is_unregister(&self) -> bool;
}

macro_rules! impl_unregister {
    ($msg:ty, $field:ident, $oneof:ty) => {
        impl $crate::grammar::Unregister for $msg {
            fn is_unregister(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::Unregister(_)))
            }
        }
    };
}

pub(crate) use {impl_ack, impl_cancel, impl_chunk, impl_unregister};
