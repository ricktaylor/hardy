//! A packet tunnel over a datagram link: the crate carrying payloads that
//! are not bundles, as an IP-over-UDP tunnel would.  Inner packets range
//! from small to jumbo, so some share a datagram and some are segmented
//! across several.

mod common;

use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, UdpSocket},
    num::NonZeroUsize,
    time::Duration,
};

use bytes::Bytes;
use hardy_btpu::{
    receiver::{MaxRetainedBytes, Receiver, ReceiverConfig, ReceiverEvent},
    sender::{BundleFraming, LinkFraming, Pdu, SendId, SendKind, SendOptions, Sender},
};

use common::{max_transfer_size, receiver_config, sender_config};

/// The UDP payload of a 1500-byte IPv4 path.
const DATAGRAM: usize = 1472;

const WINDOW: u16 = 16;

/// The largest inner packet.
const MAX_PACKET: usize = 9000;

/// Inner packet sizes, cycled: an ACK-sized packet, the IPv4 minimum
/// reassembly size, a full Ethernet MTU, a jumbo frame, a tiny packet, and
/// the IPv6 minimum MTU.  The MTU and jumbo packets do not fit a datagram.
const SIZES: [usize; 6] = [40, 576, 1500, MAX_PACKET, 64, 1280];

/// Packets enqueued before each drain, so small ones share datagrams.
const BURST: usize = 6;

fn tunnel_sender() -> Sender {
    let mut config = sender_config(DATAGRAM, WINDOW);
    config.link_framing = LinkFraming::variable(BundleFraming::Message);
    Sender::new(config, 0)
}

/// A receiver that can hold a full window of damaged transfers.  The
/// default retention limit holds one transfer's worth, so on a lossy link
/// a second damaged jumbo packet would be refused as `ReceiverFull` until
/// expiry freed the first; with a small cap, budgeting for the window costs
/// little (432 KB here).
fn tunnel_receiver() -> Receiver {
    let transfers = NonZeroUsize::new(usize::from(WINDOW)).unwrap();
    Receiver::new(ReceiverConfig {
        max_retained_bytes: Some(MaxRetainedBytes::for_transfers(
            transfers,
            max_transfer_size(MAX_PACKET),
            None,
        )),
        ..receiver_config(WINDOW, MAX_PACKET)
    })
}

/// Packet `i`, `len` bytes, its content distinct from every other packet's.
fn packet(i: usize, len: usize) -> Bytes {
    (0..len)
        .map(|j| (i.wrapping_mul(31).wrapping_add(j)) as u8)
        .collect::<Vec<u8>>()
        .into()
}

fn packets(n: usize) -> Vec<Bytes> {
    (0..n).map(|i| packet(i, SIZES[i % SIZES.len()])).collect()
}

/// Carries `packets` through `sender` in bursts, handing every PDU to
/// `link`, and returns the PDUs with the packet index each one carries
/// bytes of.
fn send_all(
    sender: &mut Sender,
    packets: &[Bytes],
    mut link: impl FnMut(&Pdu),
) -> Vec<(Pdu, Vec<usize>)> {
    let mut sent = Vec::new();
    for (burst, chunk) in packets.chunks(BURST).enumerate() {
        let ids: Vec<SendId> = chunk
            .iter()
            .map(|p| sender.enqueue(p.clone(), SendOptions::default()).unwrap())
            .collect();
        while let Some(pdu) = sender.next_pdu() {
            assert!(pdu.data.len() <= DATAGRAM);
            link(&pdu);
            let carried = pdu
                .carried
                .iter()
                .map(|c| burst * BURST + ids.iter().position(|id| *id == c.id).unwrap())
                .collect();
            sent.push((pdu, carried));
        }
    }
    sent
}

/// What the receiver reported: the delivered payloads in order, and the
/// numbers of the transfers it expired.  Any other event fails the test.
#[derive(Debug, Default)]
struct Outcome {
    delivered: Vec<Bytes>,
    expired: BTreeSet<u32>,
}

impl Outcome {
    fn record(&mut self, events: Vec<ReceiverEvent>) {
        for event in events {
            match event {
                ReceiverEvent::Received { data, .. } => self.delivered.push(data),
                ReceiverEvent::TransferExpired { id } => {
                    assert!(self.expired.insert(id.transfer_number()));
                }
                other => panic!("unexpected event {other:?}"),
            }
        }
    }
}

#[test]
fn lossless_link_delivers_every_packet_in_order() {
    let packets = packets(60);
    let mut sender = tunnel_sender();
    let mut receiver = tunnel_receiver();
    let mut outcome = Outcome::default();

    let sent = send_all(&mut sender, &packets, |pdu| {
        outcome.record(receiver.receive_pdu(pdu.data.clone()))
    });

    assert_eq!(outcome.delivered, packets);
    assert!(outcome.expired.is_empty());
    // Small packets shared datagrams and large ones spanned several.
    assert!(sent.iter().any(|(_, carried)| carried.len() > 1));
    assert!(sent.len() > packets.len());
}

#[test]
fn lossy_link_delivers_exactly_the_packets_it_did_not_damage() {
    // Drop every seventh datagram.
    const DROP_EVERY: usize = 7;

    let packets = packets(60);
    let mut sender = tunnel_sender();
    let mut receiver = tunnel_receiver();
    let mut outcome = Outcome::default();

    let mut n = 0;
    let sent = send_all(&mut sender, &packets, |pdu| {
        n += 1;
        if n % DROP_EVERY != 0 {
            outcome.record(receiver.receive_pdu(pdu.data.clone()));
        }
    });

    // A packet is damaged if any datagram carrying its bytes was dropped.
    let mut damaged = BTreeSet::new();
    let mut arrived = BTreeSet::new();
    for (i, (_, carried)) in sent.iter().enumerate() {
        let target = if (i + 1) % DROP_EVERY == 0 {
            &mut damaged
        } else {
            &mut arrived
        };
        target.extend(carried.iter().copied());
    }
    let expected: Vec<Bytes> = (0..packets.len())
        .filter(|i| !damaged.contains(i))
        .map(|i| packets[i].clone())
        .collect();
    assert!(!damaged.is_empty());
    assert_eq!(outcome.delivered, expected);

    // A segmented packet that lost some datagrams but not all is held until
    // a window's worth of newer transfers expires it; an unsegmented one
    // lost whole leaves no trace.  Flush with a window of whole transfers.
    let flush: Vec<Bytes> = (0..usize::from(WINDOW))
        .map(|i| packet(packets.len() + i, 3000))
        .collect();
    send_all(&mut sender, &flush, |pdu| {
        outcome.record(receiver.receive_pdu(pdu.data.clone()))
    });

    // Segmented packets take transfer numbers in the order they were
    // enqueued, from the sender's first, 0.
    let segmented: BTreeSet<usize> = sent
        .iter()
        .flat_map(|(pdu, carried)| pdu.carried.iter().zip(carried))
        .filter(|(c, _)| c.id.kind() == SendKind::Transfer)
        .map(|(_, &i)| i)
        .collect();
    let transfer_number = |i: usize| {
        segmented
            .contains(&i)
            .then(|| u32::try_from(segmented.range(..i).count()).unwrap())
    };
    let expected_expired: BTreeSet<u32> = damaged
        .intersection(&arrived)
        .filter_map(|&i| transfer_number(i))
        .collect();
    assert!(!expected_expired.is_empty());
    assert_eq!(outcome.expired, expected_expired);
    assert_eq!(&outcome.delivered[expected.len()..], &flush[..]);
}

#[test]
fn udp_loopback_carries_every_packet() {
    let packets = packets(60);
    let mut sender = tunnel_sender();
    let mut receiver = tunnel_receiver();
    let mut outcome = Outcome::default();

    let rx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let tx = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    tx.connect(rx.local_addr().unwrap()).unwrap();
    // The timeout only bounds a regression.
    rx.set_read_timeout(Some(Duration::from_secs(30))).unwrap();

    // Lockstep, one datagram in flight, so the socket buffer cannot
    // overflow and drop one.
    let mut buf = [0; DATAGRAM];
    send_all(&mut sender, &packets, |pdu| {
        assert_eq!(tx.send(&pdu.data).unwrap(), pdu.data.len());
        let (len, from) = rx.recv_from(&mut buf).unwrap();
        assert_eq!(from, tx.local_addr().unwrap());
        outcome.record(receiver.receive_pdu(Bytes::copy_from_slice(&buf[..len])));
    });

    assert_eq!(outcome.delivered, packets);
    assert!(outcome.expired.is_empty());
}
