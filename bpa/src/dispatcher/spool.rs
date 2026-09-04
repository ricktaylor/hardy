//! The spool: the cancellable, concurrent drain of a bundle stream into the
//! store.
//!
//! [`Dispatcher::spool`] is pure rig — a spawned store-side task fed through
//! a bounded channel by a pump over the caller's borrowed stream. It knows
//! nothing about validation: each door decorates its stream first (the input
//! doors wrap a [`ValidatingReceiver`](super::validate::ValidatingReceiver)
//! over the arrival) and settles the decorator's verdict after the spool
//! returns, owning the discard of any save the verdict rejects.

use alloc::sync::Arc;

use hardy_async::{CancellationToken, channel::bounded};
use trace_err::*;

use super::Dispatcher;
use crate::{
    cla::Segment,
    stream::{CancellableReceiver, ConcatError, Receiver},
};

impl Dispatcher {
    /// Drain `stream` into the store, concurrently with whatever the caller
    /// joins this future against.
    ///
    /// A bounded channel decouples the two halves: a spawned task owns the
    /// store side (`Store::save_stream`), while this future pumps the
    /// borrowed `stream` into the channel — the channel depth is
    /// backpressure, not buffering; the drain is bounded by
    /// `max_bundle_size` as the defensive backstop. Cancelling `token`
    /// aborts both halves, even mid-park: the pump races the token on every
    /// pull. The future resolves once both settle: the pump ends at the
    /// stream's end, at a cancel, or when the store side stops pulling.
    ///
    /// `Ok` carries the storage name and total size of the saved bundle —
    /// saved, not accepted: the caller settles its stream decorator's
    /// verdict afterwards and owes `delete_data` for a save it then
    /// rejects, exactly as a canceller owes the discard of a save that
    /// raced the cancel.
    pub(super) async fn spool(
        &self,
        stream: &mut dyn Receiver<Segment>,
        token: CancellationToken,
    ) -> Result<(Arc<str>, usize), ConcatError> {
        // 32-bit: a cap beyond the address space saturates — nothing larger
        // could be spooled to RAM anyway.
        let max_size = usize::try_from(self.max_bundle_size.get()).unwrap_or(usize::MAX);
        let (seg_tx, seg_rx) = bounded::<Segment>(4);
        let task = {
            let store = self.store.clone();
            hardy_async::spawn!(self.tasks, "spool", async move {
                let mut seg_rx = seg_rx;
                store.save_stream(&mut seg_rx, max_size).await
            })
        };

        // Pump the borrowed stream into the channel.
        {
            let mut src = CancellableReceiver {
                inner: stream,
                token,
            };
            loop {
                let Ok(seg) = src.recv().await else { break };
                let last = matches!(seg, Segment::Final(_));
                if seg_tx.send(seg).await.is_err() || last {
                    break;
                }
            }
        }
        // Release the channel so a store side still pulling (an inner
        // truncation) settles rather than parking forever.
        drop(seg_tx);

        task.await.trace_expect("Spool task failed")
    }
}
