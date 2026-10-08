#![no_main]

use std::{collections::HashMap, num::NonZeroUsize, sync::Arc};

use bytes::Bytes;
use hardy_btpu::{
    budget::RetentionBudget,
    receiver::{
        Delivery, DropReason, MaxRetainedBytes, MaxSegments, MaxTransferSize, Receiver,
        ReceiverConfig, ReceiverEvent,
    },
    transfer::{TransferId, WindowSize},
};
use libfuzzer_sys::fuzz_target;

// Every 0x9F "bundle" is taken to be eight bytes long, as in the decode
// target, so the receiver's hook path is exercised too.
fn eight_byte_bundles(bytes: &[u8]) -> Option<usize> {
    (bytes.first() == Some(&0x9F) && bytes.len() >= 8).then_some(8)
}

// The input is a sequence of PDUs, each prefixed by a one-byte length, fed to
// one receiver so window, reassembly, and abandonment state accumulate.  A
// cap of 128, below the 255-byte longest PDU, makes the oversize gates
// reachable, the single-message Bundle gate included.  The first byte selects FEC
// decoding (bit 0), a segment limit derived from a 64-byte link PDU, so the
// limit rather than the per-segment charge bounds segment count (bit 1), the
// bundle-extent hook (bit 2), and a receiver-wide retention limit of the cap,
// below one transfer's allowance, so a transfer can be refused as the receiver
// being full while it is the only one held (bit 3).  Without it the default
// limit, one transfer's allowance, still makes concurrent transfers contend.
// Bit 4 selects streamed delivery, bit 5 a shared budget of half the cap,
// so a transfer can be refused as the budget being full, and bit 6 refuses
// each streamed transfer once it has released more than 32 bytes.
fuzz_target!(|data: &[u8]| {
    let flags = data.first().copied().unwrap_or_default();
    let max_transfer_size = MaxTransferSize::try_from(128).unwrap();
    let mut receiver = Receiver::new(ReceiverConfig {
        window_size: WindowSize::try_from(4).unwrap(),
        max_transfer_size,
        max_segments_per_transfer: (flags & 2 != 0)
            .then(|| MaxSegments::for_link_pdu_size(64, max_transfer_size)),
        max_retained_bytes: (flags & 8 != 0).then_some(MaxRetainedBytes::from(NonZeroUsize::from(
            max_transfer_size,
        ))),
        fec: flags & 1 != 0,
        delivery: if flags & 16 != 0 {
            Delivery::Streamed
        } else {
            Delivery::Whole
        },
    });
    if flags & 4 != 0 {
        receiver = receiver.with_bundle_extent(eight_byte_bundles);
    }
    let budget = (flags & 32 != 0).then(|| {
        Arc::new(RetentionBudget::new(
            MaxRetainedBytes::try_from(max_transfer_size.get() / 2).unwrap(),
        ))
    });
    if let Some(budget) = &budget {
        receiver = receiver.with_budget(Arc::clone(budget));
    }
    let refuse_after = (flags & 64 != 0).then_some(32);
    // Bytes released by each started, unfinished streamed transfer.
    let mut streams: HashMap<TransferId, usize> = HashMap::new();
    let mut events = Vec::new();
    let mut rest = data.get(1..).unwrap_or_default();
    while let Some((&len, tail)) = rest.split_first() {
        let len = usize::from(len).min(tail.len());
        let (pdu, tail) = tail.split_at(len);
        receiver.receive_pdu_into(Bytes::copy_from_slice(pdu), &mut events);
        for (i, event) in events.iter().enumerate() {
            match event {
                // A delivered bundle is never empty and never over the cap.
                ReceiverEvent::Received { data, .. } => {
                    assert!(!data.is_empty());
                    assert!(data.len() <= max_transfer_size.get());
                }
                // A PDU-level fault ends the PDU, so it is reported once,
                // last.
                ReceiverEvent::MalformedPdu { .. } => assert_eq!(i, events.len() - 1),
                // A streamed transfer starts once, releases data only
                // between its start and its end, and never more than the
                // cap.  Ids are never reused, even across a reset.
                ReceiverEvent::TransferStarted { id, .. } => {
                    assert!(streams.insert(*id, 0).is_none());
                }
                ReceiverEvent::TransferData { id, data, .. } => {
                    assert!(!data.is_empty());
                    let released = streams.get_mut(id).unwrap();
                    *released += data.len();
                    assert!(*released <= max_transfer_size.get());
                }
                ReceiverEvent::TransferFinished { id, data, .. } => {
                    let released = streams.remove(id).unwrap() + data.len();
                    assert!(released > 0 && released <= max_transfer_size.get());
                }
                // A dropped message has an id exactly when its number is
                // inside the window, and the id names that number.
                ReceiverEvent::MessageDropped {
                    transfer_number,
                    id,
                    reason,
                } => {
                    let outside = matches!(
                        reason,
                        DropReason::OutsideWindow | DropReason::UnknownTransfer
                    );
                    match id {
                        None => assert!(outside),
                        Some(id) => {
                            assert!(!outside);
                            assert_eq!(id.transfer_number(), *transfer_number);
                        }
                    }
                }
                ReceiverEvent::TransferCancelled { id }
                | ReceiverEvent::TransferExpired { id }
                | ReceiverEvent::TransferRejected { id, .. } => {
                    streams.remove(id);
                }
                _ => {}
            }
        }
        if let Some(limit) = refuse_after {
            let refused: Vec<_> = streams
                .iter()
                .filter(|&(_, &released)| released > limit)
                .map(|(&id, _)| id)
                .collect();
            for id in refused {
                assert!(receiver.refuse(id));
                assert!(!receiver.refuse(id));
                streams.remove(&id);
            }
        }
        // The receiver is the budget's only user, so it holds the budget's
        // whole charge, which never exceeds the limit.
        if let Some(budget) = &budget {
            assert_eq!(budget.used(), receiver.retained_bytes());
            assert!(budget.used() <= budget.limit().get());
        }
        rest = tail;
    }
});
