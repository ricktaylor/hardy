//! Bundle IDs and send handles.

use core::fmt;

#[cfg(doc)]
use super::{BundleFraming, Carried, Error, Pdu, Sender};
use crate::owner::Owner;

/// A bundle being pushed into a [`Sender`], from [`Sender::begin`] until
/// it is ended by [`Sender::finish`] or [`Sender::cancel`].
///
/// **Dropping a handle does not cancel its bundle.**  The handle is a token,
/// not a borrow of the sender, so it cannot reach the sender when dropped.
/// A bundle whose producer gives up stays queued, supplying nothing, and a
/// segmented one holds its window slot, until the CLA calls
/// `cancel(handle)`.  A CLA that keeps the sender in `Arc<Mutex<_>>` can
/// wrap the handle in a guard of its own that cancels on drop.  A lost
/// handle's bundle is still listed by [`Sender::outstanding`], and can be
/// cancelled by its ID.
///
/// The handle counts the bytes pushed through it, so [`Sender::push`]
/// refuses an overrun and [`Sender::finish`] detects an underrun.  It
/// converts into the bundle's [`SendId`], consuming it, which is how
/// `cancel(handle)` ends it.  A handle belongs to the sender that issued
/// it: any other sender refuses it with [`Error::ForeignHandle`].
#[must_use = "dropping a send handle does not cancel its bundle; finish or cancel it"]
#[derive(Debug, PartialEq, Eq)]
pub struct SendHandle {
    pub(super) id: SendId,
    pub(super) total_len: usize,
    pub(super) pushed: usize,
}

impl SendHandle {
    /// The ID under which [`Pdu::carried`] reports the bundle.
    pub fn id(&self) -> SendId {
        self.id
    }

    /// The bundle's length, as given to [`Sender::begin`].
    pub fn total_len(&self) -> usize {
        self.total_len
    }

    /// The bytes pushed so far.
    pub fn pushed(&self) -> usize {
        self.pushed
    }
}

impl From<SendHandle> for SendId {
    fn from(handle: SendHandle) -> Self {
        handle.id
    }
}

/// Identifies a bundle from [`Sender::enqueue`] or [`Sender::begin`] until
/// its last bytes leave in a PDU, naming it in the [`Carried`] entries of
/// every PDU that carries part of it.
///
/// [`Self::kind`] reports how the bundle travels.  A segmented bundle's ID
/// holds its transfer number, which is outstanding in the window until its
/// End is packed or it is cancelled, so no two queued transfers share one.
/// Bundle Messages and bare frames draw from one separate `u32` counter
/// that advances with every such enqueue and wraps at `u32::MAX`; their
/// IDs are unique while queued unless 2³² unsegmented bundles are queued
/// at once.
///
/// Any ID may be passed back to [`Sender::cancel`] or
/// [`Sender::is_outstanding`].  The ID is opaque, with no public
/// constructor, so a caller names a bundle only by an ID the sender issued,
/// never by a wire value such as a transfer number.  Its `Debug` output
/// shows the kind and the number, for logs.
///
/// Each ID also carries a tag naming the sender that issued it, unique
/// among the senders of the process, so IDs from two senders never compare
/// equal even where their kinds and numbers match.  Passing one sender's ID
/// to another is a bug in the caller: a debug build panics on it, and a
/// release build treats it as naming nothing.  The ID is 16 bytes, the
/// number and its kind beside the 64-bit tag.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SendId {
    /// The sender that issued the ID.
    pub(super) owner: Owner,
    pub(super) local: LocalId,
}

/// A bundle's ID within its sender: what the queue keys bundles by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LocalId {
    /// The transfer number.
    Transfer(u32),
    /// A value of the unsegmented-bundle counter.
    Message(u32),
    /// As for `Message`.
    Bare(u32),
}

impl SendId {
    pub(super) const fn new(owner: Owner, local: LocalId) -> Self {
        Self { owner, local }
    }

    /// How the bundle travels.
    pub const fn kind(self) -> SendKind {
        match self.local {
            LocalId::Transfer(_) => SendKind::Transfer,
            LocalId::Message(_) => SendKind::Message,
            LocalId::Bare(_) => SendKind::Bare,
        }
    }
}

impl fmt::Debug for SendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.local.fmt(f)
    }
}

/// How a bundle travels, as [`SendId::kind`] reports it.  Fixed when the
/// bundle is enqueued or begun.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SendKind {
    /// Segmented into a transfer.  Its window slot is released by the
    /// sender itself once the transfer's End message has been packed into
    /// a PDU.
    Transfer,
    /// Sent as a single Bundle Message.
    Message,
    /// Sent as a bare bundle frame in a PDU of its own (see
    /// [`BundleFraming::Bare`]).
    Bare,
}

// The tag fits the padding beside the `u32`, so an ID costs what a `u64`
// would; `Carried` lists rely on that staying true.
const _: () = assert!(size_of::<SendId>() == 16);
