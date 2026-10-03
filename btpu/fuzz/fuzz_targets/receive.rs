#![no_main]

use std::num::NonZeroUsize;

use bytes::Bytes;
use hardy_btpu::{
    receiver::{
        MaxRetainedBytes, MaxSegments, MaxTransferSize, Receiver, ReceiverConfig, ReceiverEvent,
    },
    transfer::WindowSize,
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
    });
    if flags & 4 != 0 {
        receiver = receiver.with_bundle_extent(eight_byte_bundles);
    }
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
                _ => {}
            }
        }
        rest = tail;
    }
});
