//! The transmit contract: reserve, write in place, send; and the header
//! layer that shows a stack composing over it.

mod common;

use std::{sync::Arc, task::Poll};

use hardy_frame::{FrameTx, Permit, SendError};

use common::{HEADER, Header, Slot, fixed, poll_once, ready, size};

#[test]
fn send_copies_the_frame_into_the_slot_and_delivers_it() {
    let link = Slot::new(size(1, 8));
    assert_eq!(ready(link.send(b"abc")), Ok(()));
    assert_eq!(ready(link.send(b"defgh")), Ok(()));
    assert_eq!(link.sent(), vec![b"abc".to_vec(), b"defgh".to_vec()]);
}

#[test]
fn a_frame_outside_the_size_is_refused_without_waiting_for_room() {
    let link = Slot::new(size(4, 8));
    // Closed, so a send that reserved would wait forever.
    link.close();
    assert_eq!(
        ready(link.send(&[0; 9])),
        Err(SendError::BadLength {
            len: 9,
            size: size(4, 8)
        })
    );
    assert_eq!(
        ready(link.send(&[0; 3])),
        Err(SendError::BadLength {
            len: 3,
            size: size(4, 8)
        })
    );
    assert!(link.sent().is_empty());
}

#[test]
fn a_permit_lends_the_longest_frame_and_sends_a_prefix_of_it() {
    let link = Slot::new(size(1, 8));
    let mut permit = ready(link.reserve());
    assert_eq!(permit.as_mut().len(), 8);
    permit.as_mut()[..2].copy_from_slice(b"hi");
    assert_eq!(permit.send(2), Ok(()));
    assert_eq!(link.sent(), vec![b"hi".to_vec()]);
}

#[test]
fn a_permit_refuses_a_length_outside_the_size() {
    let link = Slot::new(size(4, 8));
    let permit = ready(link.reserve());
    assert_eq!(
        permit.send(2),
        Err(SendError::BadLength {
            len: 2,
            size: size(4, 8)
        })
    );
    assert!(link.sent().is_empty());
}

#[test]
fn a_permit_dropped_unsent_sends_nothing_and_returns_its_room() {
    let link = Slot::new(fixed(4));
    let mut permit = ready(link.reserve());
    permit.as_mut().copy_from_slice(b"lost");
    drop(permit);
    assert!(link.sent().is_empty());
    assert!(poll_once(link.reserve()).is_ready());
}

#[test]
fn a_held_permit_makes_the_next_reservation_wait() {
    let link = Slot::new(fixed(4));
    let mut permit = ready(link.reserve());
    assert!(poll_once(link.reserve()).is_pending());
    permit.as_mut().copy_from_slice(b"held");
    assert_eq!(permit.send(4), Ok(()));
    assert!(poll_once(link.reserve()).is_ready());
}

#[test]
fn a_closed_half_never_grants_again() {
    let link = Slot::new(fixed(4));
    link.close();
    assert!(poll_once(link.reserve()).is_pending());
    assert!(poll_once(link.reserve()).is_pending());
    assert!(poll_once(link.send(b"four")).is_pending());
    assert!(link.sent().is_empty());
}

#[test]
fn a_refused_frame_is_dropped_and_the_half_carries_on() {
    let link = Slot::new(fixed(4));
    link.refuse(true);
    assert_eq!(ready(link.send(b"gone")), Err(SendError::Dropped));
    link.refuse(false);
    assert_eq!(ready(link.send(b"kept")), Ok(()));
    assert_eq!(link.sent(), vec![b"kept".to_vec()]);
}

#[test]
fn a_shared_half_is_the_half_it_shares() {
    let link = Arc::new(Slot::new(size(1, 8)));
    let borrowed = &*link;
    assert_eq!(borrowed.frame_size(), link.frame_size());
    assert_eq!(ready(borrowed.send(b"by ref")), Ok(()));
    assert_eq!(ready(link.send(b"by arc")), Ok(()));
    let mut permit = ready(FrameTx::reserve(&link));
    permit.as_mut()[..1].copy_from_slice(b"!");
    assert_eq!(permit.send(1), Ok(()));
    assert_eq!(
        link.sent(),
        vec![b"by ref".to_vec(), b"by arc".to_vec(), b"!".to_vec()]
    );
}

#[test]
fn a_header_layer_offers_the_size_behind_its_header() {
    let layer = Header::new(Slot::new(size(46, 100))).unwrap();
    assert_eq!(layer.frame_size(), size(46 - HEADER, 100 - HEADER));
    assert!(Header::new(Slot::new(fixed(HEADER))).is_none());
}

#[test]
fn a_header_layer_writes_its_header_in_front_of_the_payload_in_place() {
    let link = Arc::new(Slot::new(size(46, 100)));
    let layer = Header::new(Arc::clone(&link)).unwrap();
    let mut permit = ready(layer.reserve());
    assert_eq!(permit.as_mut().len(), 100 - HEADER);
    let payload = [0xAB; 50];
    permit.as_mut()[..payload.len()].copy_from_slice(&payload);
    assert_eq!(permit.send(payload.len()), Ok(()));
    let mut expected = vec![0, 50];
    expected.extend_from_slice(&payload);
    assert_eq!(link.sent(), vec![expected]);
}

#[test]
fn a_header_layer_passes_the_refusal_below_up_unchanged() {
    let link = Arc::new(Slot::new(size(46, 100)));
    let layer = Header::new(Arc::clone(&link)).unwrap();
    link.refuse(true);
    assert_eq!(ready(layer.send(&[1; 44])), Err(SendError::Dropped));
    // The layer's own minimum keeps the frame below at its minimum.
    link.refuse(false);
    assert_eq!(ready(layer.send(&[1; 44])), Ok(()));
    assert_eq!(link.sent()[0].len(), 46);
    assert_eq!(
        ready(layer.send(&[1; 43])),
        Err(SendError::BadLength {
            len: 43,
            size: size(44, 98)
        })
    );
}

#[test]
fn a_waiting_reservation_below_keeps_the_layer_waiting() {
    let link = Arc::new(Slot::new(size(46, 100)));
    let layer = Header::new(Arc::clone(&link)).unwrap();
    let held = ready(link.reserve());
    assert!(matches!(poll_once(layer.reserve()), Poll::Pending));
    drop(held);
    assert!(poll_once(layer.reserve()).is_ready());
}
