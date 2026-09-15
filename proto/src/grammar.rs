//! The chunked-transfer grammar, as capabilities of the generated message
//! types.
//!
//! Every streaming call in every API speaks one vocabulary: a run of `chunk`
//! messages ended by `last_chunk`, an in-stream `cancel` from the client, an
//! `ack` that commits a collection, and on the session stream a `register`
//! request answered by a `registration` event and ended by an `unregister`.
//! The generated message
//! types spell these `oneof` variants differently per API, so each trait here
//! is one capability and each `impl_*` macro implements it for a message type
//! by naming the `oneof` field and variant. The transport code is written once
//! against the traits.

use hardy_bpa::stream::Segment;
use tonic::Status;

/// A message that can carry one segment of a transfer.
pub trait Chunk: Sized {
    /// Wraps `segment` as a `chunk` message, or as a `last_chunk` message for
    /// [`Segment::Final`].
    fn chunk(segment: Segment) -> Self;

    /// Unwraps a `chunk` or `last_chunk` message into its segment.
    ///
    /// Returns any other message back as the error.
    fn into_chunk(self) -> Result<Segment, Self>;
}

/// A response channel item carries a chunk when it is the message rather
/// than the status the call ends with.
impl<M: Chunk> Chunk for Result<M, Status> {
    fn chunk(segment: Segment) -> Self {
        Ok(M::chunk(segment))
    }

    fn into_chunk(self) -> Result<Segment, Self> {
        match self {
            Ok(message) => message.into_chunk().map_err(Ok),
            Err(status) => Err(Err(status)),
        }
    }
}

/// Implements [`Chunk`] for `$msg`, whose `$field` is the `$oneof` holding its
/// `Chunk` and `LastChunk` variants.
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

            fn into_chunk(self) -> Result<hardy_bpa::stream::Segment, Self> {
                type Oneof = $oneof;
                match self.$field {
                    Some(Oneof::Chunk(bytes)) => Ok(hardy_bpa::stream::Segment::Next(bytes)),
                    Some(Oneof::LastChunk(bytes)) => Ok(hardy_bpa::stream::Segment::Final(bytes)),
                    other => Err(Self { $field: other }),
                }
            }
        }
    };
}

/// A request that can abandon a transfer in-stream with the client's `cancel`.
pub trait Cancel: Sized {
    /// Creates the cancel message.
    #[cfg(feature = "client")]
    fn cancel() -> Self;

    /// Returns `true` if this is the cancel message.
    #[cfg(feature = "server")]
    fn is_cancel(&self) -> bool;
}

/// Implements [`Cancel`] for `$msg`, whose `$field` is the `$oneof` holding the
/// unit variant `$cancel`.
macro_rules! impl_cancel {
    ($msg:ty, $field:ident, $oneof:ty, $cancel:ident) => {
        impl $crate::grammar::Cancel for $msg {
            #[cfg(feature = "client")]
            fn cancel() -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(Oneof::$cancel(())),
                }
            }

            #[cfg(feature = "server")]
            fn is_cancel(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::$cancel(_)))
            }
        }
    };
}

/// A message that can commit a collection once its last chunk has arrived.
pub trait Ack: Sized {
    /// Creates the ack message.
    #[cfg(feature = "client")]
    fn ack() -> Self;

    /// Returns `true` if this is the ack message.
    #[cfg(feature = "server")]
    fn is_ack(&self) -> bool;
}

/// Implements [`Ack`] for `$msg`, whose `$field` is the `$oneof` holding the
/// unit variant `$ack`.
macro_rules! impl_ack {
    ($msg:ty, $field:ident, $oneof:ty, $ack:ident) => {
        impl $crate::grammar::Ack for $msg {
            #[cfg(feature = "client")]
            fn ack() -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(Oneof::$ack(())),
                }
            }

            #[cfg(feature = "server")]
            fn is_ack(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::$ack(_)))
            }
        }
    };
}

/// A session request that may open the registration.
#[cfg(feature = "server")]
pub trait Register: Sized {
    /// The API's `Register` message.
    type Registration;

    /// Unwraps the `Register` message that opens a session.
    ///
    /// Returns `None` for any other request.
    fn into_register(self) -> Option<Self::Registration>;
}

/// Implements [`Register`] for `$msg`, whose `$field` is the `$oneof` holding a
/// `Register` variant of type `$registration`.
#[cfg(feature = "server")]
macro_rules! impl_register {
    ($msg:ty, $field:ident, $oneof:ty, $registration:ty) => {
        impl $crate::grammar::Register for $msg {
            type Registration = $registration;

            fn into_register(self) -> Option<Self::Registration> {
                type Oneof = $oneof;
                match self.$field {
                    Some(Oneof::Register(registration)) => Some(registration),
                    _ => None,
                }
            }
        }
    };
}

/// A session event that may carry the result of the registration.
#[cfg(feature = "client")]
pub trait Registration: Sized {
    /// The API's `Registration` message.
    type Registration;

    /// Unwraps the `Registration` event that answers a `Register`.
    ///
    /// Returns `None` for any other event.
    fn into_registration(self) -> Option<Self::Registration>;
}

/// Implements [`Registration`] for `$msg`, whose `$field` is the `$oneof`
/// holding a `Registration` variant of type `$registration`.
#[cfg(feature = "client")]
macro_rules! impl_registration {
    ($msg:ty, $field:ident, $oneof:ty, $registration:ty) => {
        impl $crate::grammar::Registration for $msg {
            type Registration = $registration;

            fn into_registration(self) -> Option<Self::Registration> {
                type Oneof = $oneof;
                match self.$field {
                    Some(Oneof::Registration(registration)) => Some(registration),
                    _ => None,
                }
            }
        }
    };
}

/// A session request that may end the registration.
pub trait Unregister: Sized {
    /// Creates the `Unregister` message.
    #[cfg(feature = "client")]
    fn unregister() -> Self;

    /// Returns `true` if this is the `Unregister` message.
    #[cfg(feature = "server")]
    fn is_unregister(&self) -> bool;
}

/// Implements [`Unregister`] for `$msg`, whose `$field` is the `$oneof` holding
/// an `Unregister` variant.
macro_rules! impl_unregister {
    ($msg:ty, $field:ident, $oneof:ty) => {
        impl $crate::grammar::Unregister for $msg {
            #[cfg(feature = "client")]
            fn unregister() -> Self {
                type Oneof = $oneof;
                Self {
                    $field: Some(Oneof::Unregister(Default::default())),
                }
            }

            #[cfg(feature = "server")]
            fn is_unregister(&self) -> bool {
                type Oneof = $oneof;
                matches!(self.$field, Some(Oneof::Unregister(_)))
            }
        }
    };
}

#[cfg(feature = "server")]
pub(crate) use impl_register;
#[cfg(feature = "client")]
pub(crate) use impl_registration;
pub(crate) use {impl_ack, impl_cancel, impl_chunk, impl_unregister};
