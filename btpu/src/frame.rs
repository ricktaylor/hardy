//! BTP-U over a frame service (the `frame` feature): a [`Sender`] bound to
//! a transmit half and a [`Receiver`] bound to a receive half, one type per
//! stack.
//!
//! BTP-U is unidirectional, so the two are independent: a node that only
//! sends has a `Sender<Tx>` and nothing else, and a node that does both has
//! one of each, over whatever halves its stack offers.  Both are plain
//! state machines polled by their owner, with no task, no lock and no
//! allocation per frame of their own: the sender packs a PDU only into a
//! frame it already holds, and the receiver pulls frames as its events are
//! pulled.

use alloc::{collections::VecDeque, vec::Vec};
use core::{
    pin::Pin,
    task::{Context, Poll, ready},
};

use bytes::Bytes;
use futures_core::Stream;
use hardy_frame::{FrameRx, FrameSize, FrameTx, Permit, SendError};
use pin_project_lite::pin_project;

use crate::{
    OutOfRange,
    // Qualified at use: this module's `Receiver` and `Sender` are the ones
    // over a half, and the sans-IO cores keep their own names.
    receiver,
    receiver::ReceiverEvent,
    sender,
    sender::{CarriedList, NextPduOptions, Result, SendHandle, SendId, SendOptions, SenderConfig},
    transfer::TransferId,
};

/// A BTP-U sender over a transmit half: the sans-IO [`sender::Sender`]
/// bound to the half it packs into.
///
/// The PDU size is the half's longest frame and the link framing follows
/// the half's [`FrameSize`] (see [`SenderConfig::with_frame_size`]), both
/// taken at construction, so a stack the protocol cannot use is refused
/// when it is assembled.  Producers queue bundles through the same calls as the core
/// sender, waiting on [`poll_ready`](Self::poll_ready) for admission; the
/// owner drains with [`poll_send`](Self::poll_send), which waits for a
/// packable PDU, then for a frame, packs the PDU into the frame's memory
/// and sends it, in that order, so nothing is packed without somewhere to
/// go.  A CLA with a clock flushes with [`poll_flush`](Self::poll_flush).
pub struct Sender<Tx> {
    core: sender::Sender,
    tx: Tx,
}

impl<Tx: FrameTx> Sender<Tx> {
    /// A sender over `tx`, configured as `config` says once sized for the
    /// half's frames ([`SenderConfig::with_frame_size`]), numbering its
    /// transfers from `initial_transfer_number` (see
    /// [`sender::Sender::new`]).
    ///
    /// # Errors
    ///
    /// [`OutOfRange`] if the half's longest frame is not a valid
    /// [`sender::PduSize`].
    pub fn new(
        config: SenderConfig,
        tx: Tx,
        initial_transfer_number: u32,
    ) -> core::result::Result<Self, OutOfRange> {
        let config = config.with_frame_size(tx.frame_size())?;
        Ok(Self {
            core: sender::Sender::new(config, initial_transfer_number),
            tx,
        })
    }

    /// The sender and the half, unbound.
    pub fn into_parts(self) -> (sender::Sender, Tx) {
        (self.core, self.tx)
    }

    /// The range of frame lengths the half carries.
    pub fn frame_size(&self) -> FrameSize {
        self.tx.frame_size()
    }

    /// See [`sender::Sender::poll_ready`].
    pub fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self.core.poll_ready(cx)
    }

    /// See [`sender::Sender::enqueue`].
    ///
    /// # Errors
    ///
    /// As for [`sender::Sender::enqueue`].
    pub fn enqueue(&mut self, data: Bytes, options: SendOptions) -> Result<SendId> {
        self.core.enqueue(data, options)
    }

    /// See [`sender::Sender::begin`].
    ///
    /// # Errors
    ///
    /// As for [`sender::Sender::begin`].
    pub fn begin(&mut self, total_len: usize, options: SendOptions) -> Result<SendHandle> {
        self.core.begin(total_len, options)
    }

    /// See [`sender::Sender::push`].
    ///
    /// # Errors
    ///
    /// As for [`sender::Sender::push`].
    pub fn push(&mut self, handle: &mut SendHandle, chunk: Bytes) -> Result<()> {
        self.core.push(handle, chunk)
    }

    /// See [`sender::Sender::is_push_ready`].
    pub fn is_push_ready(&self, handle: &SendHandle) -> bool {
        self.core.is_push_ready(handle)
    }

    /// See [`sender::Sender::finish`].
    ///
    /// # Errors
    ///
    /// As for [`sender::Sender::finish`].
    pub fn finish(&mut self, handle: SendHandle) -> Result<SendId> {
        self.core.finish(handle)
    }

    /// See [`sender::Sender::cancel`].
    pub fn cancel(&mut self, id: impl Into<SendId>) -> bool {
        self.core.cancel(id)
    }

    /// See [`sender::Sender::is_outstanding`].
    pub fn is_outstanding(&self, id: SendId) -> bool {
        self.core.is_outstanding(id)
    }

    /// See [`sender::Sender::has_pending`].
    pub fn has_pending(&self) -> bool {
        self.core.has_pending()
    }

    /// Send the next PDU as one frame: wait until a PDU can be packed, then
    /// for a frame below, pack the PDU into the frame's memory and send it.
    ///
    /// `Ready` once per frame, with `carried` holding the bundles the frame
    /// carried (as [`sender::Sender::next_pdu_into`] fills it) and the
    /// result of the send: `Ok` means a layer below accepted the frame,
    /// and an `Err` means the frame, and with it every bundle `carried`
    /// names, was lost, since the sender has already let them go.
    /// `Pending` while there is nothing to send or no room below, with the
    /// task woken when either changes; frames leave in pack order.
    pub fn poll_send(
        &mut self,
        cx: &mut Context<'_>,
        carried: &mut CarriedList,
    ) -> Poll<core::result::Result<(), SendError>> {
        loop {
            ready!(self.core.poll_pdu_ready(cx));
            let mut permit = ready!(self.tx.poll_reserve(cx));
            match pack(
                &mut self.core,
                carried,
                permit.as_mut(),
                NextPduOptions::default(),
            ) {
                Some(len) => return Poll::Ready(permit.send(len)),
                // Cancelled since the sender was ready: the frame goes back
                // unsent, and the wait starts again.
                None => drop(permit),
            }
        }
    }

    /// Send what waiting transfers have buffered, as [`poll_send`](Self::poll_send)
    /// with [`NextPduOptions::flush`]: for a CLA whose clock says a producer
    /// has gone quiet, or whose link offers a frame that would otherwise
    /// carry only padding.
    ///
    /// Waits only for a frame below, never for the sender.  `Ready(Ok(()))`
    /// with `carried` empty means there was nothing to flush, and the frame
    /// went back unsent.
    pub fn poll_flush(
        &mut self,
        cx: &mut Context<'_>,
        carried: &mut CarriedList,
    ) -> Poll<core::result::Result<(), SendError>> {
        let mut permit = ready!(self.tx.poll_reserve(cx));
        let flush = NextPduOptions { flush: true };
        Poll::Ready(
            match pack(&mut self.core, carried, permit.as_mut(), flush) {
                Some(len) => permit.send(len),
                None => Ok(()),
            },
        )
    }
}

/// Pack `core`'s next PDU into `frame`, which a permit of its half lends.
fn pack(
    core: &mut sender::Sender,
    carried: &mut CarriedList,
    frame: &mut [u8],
    options: NextPduOptions,
) -> Option<usize> {
    core.next_pdu_write_with(carried, frame, options)
        .expect("a permit lends the longest frame, which is the PDU size")
}

pin_project! {
    /// A BTP-U receiver over a receive half: the sans-IO
    /// [`receiver::Receiver`] fed from the half, as a [`Stream`] of its
    /// events.
    ///
    /// Frames are pulled only as events are pulled, so a consumer that
    /// stops taking events stops taking frames, which is the half's
    /// backpressure.  The stream ends when the half does.
    pub struct Receiver<Rx> {
        core: receiver::Receiver,
        #[pin]
        rx: Rx,
        // Events of the last frame not yet yielded, in order.
        events: VecDeque<ReceiverEvent>,
        // The list each frame's events are received into, reused.
        received: Vec<ReceiverEvent>,
    }
}

impl<Rx: FrameRx> Receiver<Rx> {
    /// `core`, built as the CLA wants it (its hook and budget included),
    /// fed from `rx`.
    pub fn new(core: receiver::Receiver, rx: Rx) -> Self {
        Self {
            core,
            rx,
            events: VecDeque::new(),
            received: Vec::new(),
        }
    }

    /// The range of frame lengths the half carries.
    pub fn frame_size(&self) -> FrameSize {
        self.rx.frame_size()
    }

    /// The receiver and the half, unbound.  Events of a frame already
    /// pulled but not yet yielded are dropped.
    pub fn into_parts(self) -> (receiver::Receiver, Rx) {
        (self.core, self.rx)
    }

    /// See [`receiver::Receiver::refuse`].
    pub fn refuse(&mut self, id: TransferId) -> bool {
        self.core.refuse(id)
    }

    /// See [`receiver::Receiver::reset`].
    pub fn reset(&mut self) {
        self.core.reset();
    }
}

impl<Rx: FrameRx> Stream for Receiver<Rx> {
    type Item = ReceiverEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<ReceiverEvent>> {
        let mut this = self.project();
        loop {
            if let Some(event) = this.events.pop_front() {
                return Poll::Ready(Some(event));
            }
            let Some(frame) = ready!(this.rx.as_mut().poll_next(cx)) else {
                return Poll::Ready(None);
            };
            this.core.receive_pdu_into(frame, this.received);
            this.events.extend(this.received.drain(..));
        }
    }
}
