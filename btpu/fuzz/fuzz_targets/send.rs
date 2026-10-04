#![no_main]

use std::collections::HashSet;

use bytes::Bytes;
use hardy_btpu::{
    codec::hint::{HintItem, HintType, HintValue, Hints},
    receiver::{
        MaxRetainedBytes, MaxSegments, MaxTransferSize, Receiver, ReceiverConfig, ReceiverEvent,
    },
    sender::{
        BundleFraming, Error, LinkFraming, NextPduOptions, PduSize, SegmentCutStrategy, SendHandle,
        SendId, SendKind, SendOptions, SendQueueHighWatermark, Sender, SenderConfig,
    },
    transfer::WindowSize,
};
use libfuzzer_sys::fuzz_target;

const WINDOW: u16 = 4;
const MAX_BUNDLE: usize = 1500;

// The input's bytes, read front to back; reading past the end yields `None`.
struct Input<'a>(&'a [u8]);

impl Input<'_> {
    fn byte(&mut self) -> Option<u8> {
        let (&b, rest) = self.0.split_first()?;
        self.0 = rest;
        Some(b)
    }

    // A bundle length from two bytes, at least the five `bundle` needs.
    fn bundle_len(&mut self) -> Option<usize> {
        let n = u16::from_be_bytes([self.byte()?, self.byte()?]);
        Some(5 + usize::from(n) % (MAX_BUNDLE - 4))
    }
}

// A bundle of `len` bytes, distinct from every other the run makes: a
// bundle-reserved first byte, so a bare frame may carry it, then `serial`.
fn bundle(serial: u32, len: usize) -> Bytes {
    let mut data = vec![0x9F];
    data.extend_from_slice(&serial.to_be_bytes());
    data.extend((5..len).map(|i| i as u8));
    Bytes::from(data)
}

// Options carrying an unknown hint, so segment 0 has hints besides the
// Bundle Length, or none.
fn options(hinted: bool) -> SendOptions {
    let mut hints = Hints::new();
    if hinted {
        hints.insert(HintItem::Unknown {
            hint_type: HintType::new(0x41).unwrap(),
            value: HintValue::new(Bytes::from_static(b"hint")).unwrap(),
        });
    }
    SendOptions { hints }
}

// A bundle begun and not yet finished or cancelled.
struct Open {
    handle: SendHandle,
    data: Bytes,
}

// What the run expects of the sender, checked as PDUs are drained into a
// receiver over a lossless link.
struct Run {
    sender: Sender,
    receiver: Receiver,
    pdu_size: usize,
    min_pdu_len: usize,
    fixed_size: bool,
    serial: u32,
    open: Vec<Open>,
    // Bundles finished or enqueued whose last bytes are not yet packed,
    // which `cancel` may still reach by ID.
    queued: Vec<(SendId, Bytes)>,
    // IDs that may appear in a PDU's carried list: begun or enqueued, not
    // cancelled, and not yet completed.
    live: HashSet<SendId>,
    // Bundles that must be delivered by the end of the run.
    expected: HashSet<Bytes>,
    cancelled: HashSet<Bytes>,
    delivered: HashSet<Bytes>,
}

impl Run {
    fn new(flags: u8, pdu_byte: u8) -> Self {
        let (link_framing, fixed_size) = match flags & 3 {
            0 => (LinkFraming::FixedSize, true),
            framing => (
                LinkFraming::Variable {
                    bundle_framing: if framing == 3 {
                        BundleFraming::Bare
                    } else {
                        BundleFraming::Message
                    },
                    min_pdu_len: if framing == 2 { 46 } else { 0 },
                },
                false,
            ),
        };
        let pdu_size = 24 + usize::from(pdu_byte);
        let min_pdu_len = match link_framing {
            LinkFraming::FixedSize => pdu_size,
            LinkFraming::Variable { min_pdu_len, .. } => min_pdu_len.min(pdu_size),
        };
        let config = SenderConfig {
            pdu_size: PduSize::try_from(pdu_size).unwrap(),
            window_size: WindowSize::try_from(WINDOW).unwrap(),
            // Below one segment, so pushes past the bound are common.
            send_queue_high_watermark: SendQueueHighWatermark::try_from(if flags & 4 != 0 {
                16
            } else {
                4096
            })
            .unwrap(),
            link_framing,
            segment_cut_strategy: if flags & 8 != 0 {
                SegmentCutStrategy::Half
            } else {
                SegmentCutStrategy::Full
            },
        };
        // Transfer numbers roll over within the run.
        let initial = if flags & 16 != 0 { u32::MAX - 1 } else { 0 };
        let receiver = Receiver::new(ReceiverConfig {
            window_size: WindowSize::try_from(WINDOW).unwrap(),
            max_transfer_size: MaxTransferSize::try_from(2 * MAX_BUNDLE).unwrap(),
            // Flushed segments may carry one byte each.
            max_segments_per_transfer: Some(MaxSegments::MAX),
            max_retained_bytes: Some(MaxRetainedBytes::try_from(1 << 24).unwrap()),
            ..ReceiverConfig::default()
        });
        Self {
            sender: Sender::new(config, initial),
            receiver,
            pdu_size,
            min_pdu_len,
            fixed_size,
            serial: 0,
            open: Vec::new(),
            queued: Vec::new(),
            live: HashSet::new(),
            expected: HashSet::new(),
            cancelled: HashSet::new(),
            delivered: HashSet::new(),
        }
    }

    fn next_bundle(&mut self, len: usize) -> Bytes {
        self.serial += 1;
        bundle(self.serial, len)
    }

    // Admission refuses only for want of a window slot or a PDU able to
    // carry the segments.
    fn check_refusal(e: Error) {
        assert!(
            matches!(
                e,
                Error::Window(_) | Error::PduTooSmall { .. } | Error::TooManySegments { .. }
            ),
            "{e:?}"
        );
    }

    fn begin(&mut self, len: usize, hinted: bool) {
        let data = self.next_bundle(len);
        match self.sender.begin(len, options(hinted)) {
            Ok(handle) => {
                assert!(self.live.insert(handle.id()));
                self.open.push(Open { handle, data });
            }
            Err(e) => Self::check_refusal(e),
        }
    }

    fn enqueue(&mut self, len: usize, hinted: bool) {
        let data = self.next_bundle(len);
        match self.sender.enqueue(data.clone(), options(hinted)) {
            Ok(id) => {
                assert!(self.live.insert(id));
                self.queued.push((id, data.clone()));
                self.expected.insert(data);
            }
            Err(e) => Self::check_refusal(e),
        }
    }

    fn push(&mut self, k: usize, n: usize) {
        let open = &mut self.open[k];
        let (pushed, total) = (open.handle.pushed(), open.handle.total_len());
        let overrun = pushed + n > total;
        let chunk = if overrun {
            Bytes::from(vec![0; n])
        } else {
            open.data.slice(pushed..pushed + n)
        };
        let result = self.sender.push(&mut open.handle, chunk);
        if overrun {
            assert!(matches!(result, Err(Error::Overrun { .. })), "{result:?}");
            assert_eq!(open.handle.pushed(), pushed);
        } else {
            assert_eq!(result, Ok(()));
            assert_eq!(open.handle.pushed(), pushed + n);
        }
    }

    fn finish(&mut self, k: usize) {
        let Open { handle, data } = self.open.swap_remove(k);
        let (id, complete) = (handle.id(), handle.pushed() == handle.total_len());
        match self.sender.finish(handle) {
            Ok(finished) => {
                assert!(complete);
                assert_eq!(finished, id);
                if self.live.contains(&id) {
                    self.queued.push((id, data.clone()));
                }
                self.expected.insert(data);
            }
            Err(e) => {
                assert!(!complete);
                assert!(matches!(e, Error::Underrun { .. }), "{e:?}");
                self.cancel_bundle(id, data);
            }
        }
    }

    // Record `data`, whose ID was `id`, as cancelled: it must never arrive.
    fn cancel_bundle(&mut self, id: SendId, data: Bytes) {
        assert!(!self.delivered.contains(&data));
        self.live.remove(&id);
        self.expected.remove(&data);
        self.cancelled.insert(data);
    }

    fn cancel_open(&mut self, k: usize) {
        let Open { handle, data } = self.open.swap_remove(k);
        let id = handle.id();
        if self.sender.cancel(handle) {
            self.cancel_bundle(id, data);
        } else {
            // Only a bundle whose last bytes are packed is beyond reach.
            assert!(!self.live.contains(&id));
            self.expected.insert(data);
        }
    }

    fn cancel_queued(&mut self, k: usize) {
        let (id, data) = self.queued.swap_remove(k);
        assert!(self.sender.cancel(id), "a queued bundle is still in reach");
        self.cancel_bundle(id, data);
    }

    fn next_pdu(&mut self, options: NextPduOptions) -> bool {
        // A producer that drains whenever a push is not ready always finds
        // a PDU to drain.
        let blocked = self
            .open
            .iter()
            .any(|o| !self.sender.is_push_ready(&o.handle));
        let Some(pdu) = self.sender.next_pdu_with(options) else {
            assert!(
                !blocked,
                "a producer gated on push readiness would wait forever"
            );
            return false;
        };
        assert!(pdu.data.len() <= self.pdu_size);
        assert!(pdu.data.len() >= self.min_pdu_len);
        if self.fixed_size {
            assert_eq!(pdu.data.len(), self.pdu_size);
        }

        let mut seen = HashSet::new();
        for c in &pdu.carried {
            assert!(seen.insert(c.id), "{:?} listed twice", c.id);
            assert!(self.live.contains(&c.id), "{:?} is not live", c.id);
            if c.id.kind() != SendKind::Transfer {
                assert!(c.completes);
            }
            if c.completes {
                self.live.remove(&c.id);
                self.queued.retain(|(id, _)| *id != c.id);
            }
        }

        for event in self.receiver.receive_pdu(pdu.data) {
            match event {
                ReceiverEvent::Received { data, .. } => {
                    assert!(
                        !self.cancelled.contains(&data),
                        "a cancelled bundle arrived"
                    );
                    assert!(self.delivered.insert(data), "a bundle arrived twice");
                }
                // A transfer cancelled after its first segment was sent.
                ReceiverEvent::TransferCancelled { .. } => {}
                other => panic!("unexpected event on a lossless link: {other:?}"),
            }
        }
        true
    }

    // Push and finish every open bundle, then drain the queue.
    fn complete(mut self) {
        while let Some(open) = self.open.last() {
            let n = open.handle.total_len() - open.handle.pushed();
            self.push(self.open.len() - 1, n);
            self.finish(self.open.len() - 1);
        }
        while self.next_pdu(NextPduOptions::default()) {}
        assert!(!self.sender.has_pending());
        assert_eq!(self.sender.queued_bytes(), 0);
        assert!(self.sender.is_window_available());
        assert!(self.live.is_empty());
        assert_eq!(self.delivered, self.expected);
    }
}

// The input is two configuration bytes and a sequence of operations on one
// sender, whose PDUs are fed to a receiver over a lossless link.  The first
// byte selects the link framing (bits 0 and 1: fixed-size, variable,
// variable with a 46-byte floor, or variable with bare bundle frames), a
// send queue high watermark below one segment (bit 2), `SegmentCutStrategy::Half`
// (bit 3), and an initial transfer number that rolls over (bit 4); the
// second sets the PDU size, 24 to 279 bytes.  At the end every open bundle
// is pushed and finished and the queue drained, and exactly the bundles
// neither cancelled nor finished short must have arrived, once each.
fuzz_target!(|data: &[u8]| {
    let mut input = Input(data);
    let (Some(flags), Some(pdu_byte)) = (input.byte(), input.byte()) else {
        return;
    };
    let mut run = Run::new(flags, pdu_byte);
    while let Some(op) = input.byte() {
        let hinted = op & 0x80 != 0;
        match op % 8 {
            0 => {
                let Some(len) = input.bundle_len() else { break };
                run.begin(len, hinted);
            }
            1 => {
                let Some(len) = input.bundle_len() else { break };
                run.enqueue(len, hinted);
            }
            2 | 3 if !run.open.is_empty() => {
                let (Some(k), Some(n)) = (input.byte(), input.byte()) else {
                    break;
                };
                // Chunks up to 1020 bytes, often enough to overrun.
                let n = if op % 8 == 3 {
                    usize::from(n) * 4
                } else {
                    usize::from(n)
                };
                run.push(usize::from(k) % run.open.len(), n);
            }
            4 if !run.open.is_empty() => {
                let Some(k) = input.byte() else { break };
                run.finish(usize::from(k) % run.open.len());
            }
            5 => {
                let Some(k) = input.byte() else { break };
                if hinted && !run.open.is_empty() {
                    run.cancel_open(usize::from(k) % run.open.len());
                } else if !run.queued.is_empty() {
                    run.cancel_queued(usize::from(k) % run.queued.len());
                }
            }
            6 => {
                run.next_pdu(NextPduOptions::default());
            }
            7 => {
                run.next_pdu(NextPduOptions { flush: true });
            }
            _ => {}
        }
    }
    run.complete();
});
