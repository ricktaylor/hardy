//! The size arithmetic a stack is checked with when it is assembled.

mod common;

use hardy_frame::{FrameSize, SendError};

use common::{fixed, size};

#[test]
fn a_size_needs_at_least_one_byte_and_a_minimum_no_larger_than_its_maximum() {
    assert_eq!(FrameSize::new(0, 5), None);
    assert_eq!(FrameSize::new(5, 4), None);
    assert_eq!(FrameSize::fixed(0), None);
    assert_eq!(FrameSize::up_to(0), None);
    assert_eq!(FrameSize::new(1, 1), FrameSize::fixed(1));
    assert_eq!(FrameSize::up_to(9), FrameSize::new(1, 9));
}

#[test]
fn a_fixed_size_has_equal_bounds() {
    assert!(fixed(1115).is_fixed());
    assert_eq!(fixed(1115).min(), 1115);
    assert_eq!(fixed(1115).max(), 1115);
    assert!(!size(46, 1500).is_fixed());
}

#[test]
fn contains_is_inclusive_at_both_ends() {
    let ethernet = size(46, 1500);
    assert!(!ethernet.contains(45));
    assert!(ethernet.contains(46));
    assert!(ethernet.contains(1500));
    assert!(!ethernet.contains(1501));
    assert!(!ethernet.contains(0));
}

#[test]
fn a_service_covers_a_layer_whose_range_lies_within_its_own() {
    let udp = FrameSize::up_to(1472).unwrap();
    let aos = fixed(1115);
    assert!(udp.covers(&aos));
    assert!(!aos.covers(&udp));
    assert!(aos.covers(&aos));
    assert!(udp.covers(&size(46, 1472)));
    assert!(!size(46, 1472).covers(&udp));
    assert!(!udp.covers(&FrameSize::up_to(1473).unwrap()));
}

#[test]
fn the_size_behind_a_header_shrinks_both_bounds_but_keeps_a_byte() {
    assert_eq!(size(46, 1500).behind(2), Some(size(44, 1498)));
    assert_eq!(
        FrameSize::up_to(1500).unwrap().behind(6),
        FrameSize::up_to(1494)
    );
    assert_eq!(fixed(1115).behind(6), Some(fixed(1109)));
    assert_eq!(size(4, 100).behind(4), Some(size(1, 96)));
}

#[test]
fn a_header_that_leaves_no_room_has_no_size_behind_it() {
    assert_eq!(fixed(2).behind(2), None);
    assert_eq!(size(1, 3).behind(5), None);
}

#[test]
fn sizes_display_as_a_length_or_a_range() {
    assert_eq!(fixed(1115).to_string(), "1115");
    assert_eq!(size(46, 1500).to_string(), "46..=1500");
    assert_eq!(
        SendError::BadLength {
            len: 1501,
            size: size(46, 1500)
        }
        .to_string(),
        "a frame of 1501 bytes is outside the 46..=1500 bytes the service carries"
    );
    assert_eq!(SendError::Dropped.to_string(), "frame dropped");
}
