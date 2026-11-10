//! The receive contract: a pull stream of frames that ends when the
//! service closes.

mod common;

use bytes::Bytes;
use futures::StreamExt;
use hardy_frame::FrameRx;

use common::{Frames, ready, size};

#[test]
fn frames_are_pulled_in_order_and_the_stream_ends_with_the_service() {
    let mut rx = Frames::new(
        size(1, 4),
        [Bytes::from_static(b"one"), Bytes::from_static(b"two")],
    );
    assert_eq!(rx.frame_size(), size(1, 4));
    assert_eq!(ready(rx.next()), Some(Bytes::from_static(b"one")));
    assert_eq!(ready(rx.next()), Some(Bytes::from_static(b"two")));
    assert_eq!(ready(rx.next()), None);
}
