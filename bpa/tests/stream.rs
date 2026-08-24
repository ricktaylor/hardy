//! Segment-stream reassembly and buffering through the public `stream` API.

mod common;

use common::feed;
use hardy_bpa::{
    Bytes,
    stream::{BufferError, ConcatError, Receiver as _, Segment, buffer_stream, concat_stream},
};

#[tokio::test]
async fn concat_reassembles_multi_segment_streams() {
    let mut rx = feed(vec![
        Segment::Next(Bytes::from_static(b"he")),
        Segment::Next(Bytes::from_static(b"ll")),
        Segment::Final(Bytes::from_static(b"o")),
    ])
    .await;
    assert_eq!(
        concat_stream(&mut rx, usize::MAX).await.unwrap().as_ref(),
        b"hello"
    );
}

#[tokio::test]
async fn concat_accepts_a_trailing_empty_final() {
    let mut rx = feed(vec![
        Segment::Next(Bytes::from_static(b"data")),
        Segment::Final(Bytes::new()),
    ])
    .await;
    assert_eq!(
        concat_stream(&mut rx, usize::MAX).await.unwrap().as_ref(),
        b"data"
    );
}

#[tokio::test]
async fn concat_fails_a_truncated_stream() {
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    hardy_async::channel::Sender::send(&tx, Segment::Next(Bytes::from_static(b"part")))
        .await
        .unwrap();
    drop(tx); // no Final: the producer died mid-bundle
    assert!(matches!(
        concat_stream(&mut rx, usize::MAX).await,
        Err(ConcatError::Cancelled)
    ));
}

// A bounded(1) channel with a spawned producer: `recv` consuming is
// what lets the producer make progress, pinning the pull-driven
// backpressure contract.
#[tokio::test]
async fn concat_backpressures_a_bounded_producer() {
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    let producer = tokio::spawn(async move {
        for chunk in [&b"aa"[..], &b"bb"[..]] {
            hardy_async::channel::Sender::send(&tx, Segment::Next(Bytes::from(chunk)))
                .await
                .unwrap();
        }
        hardy_async::channel::Sender::send(&tx, Segment::Final(Bytes::from(&b"cc"[..])))
            .await
            .unwrap();
    });
    assert_eq!(
        concat_stream(&mut rx, usize::MAX).await.unwrap().as_ref(),
        b"aabbcc"
    );
    producer.await.unwrap();
}

#[tokio::test]
async fn concat_enforces_the_size_limit() {
    let mut rx = feed(vec![
        Segment::Next(Bytes::from_static(b"0123456789")),
        Segment::Final(Bytes::from_static(b"0123456789")),
    ])
    .await;
    assert!(matches!(
        concat_stream(&mut rx, 15).await,
        Err(ConcatError::TooLarge { size: 20, max: 15 })
    ));
}

#[tokio::test]
async fn a_whole_buffer_yields_a_single_final() {
    let mut rx = Bytes::from_static(b"data");
    assert!(matches!(rx.recv().await, Ok(Segment::Final(d)) if d.as_ref() == b"data"));
    // Drained: an owned buffer has no producer to lose, so it yields
    // empty Finals rather than RecvError.
    assert!(matches!(rx.recv().await, Ok(Segment::Final(d)) if d.is_empty()));
}

// An empty buffer is a legitimate empty stream, not a truncation.
#[tokio::test]
async fn an_empty_whole_buffer_is_an_empty_stream_not_a_truncation() {
    let mut rx = Bytes::new();
    assert!(matches!(rx.recv().await, Ok(Segment::Final(d)) if d.is_empty()));
}

// The single-Final path through the buffering helper is zero-copy: the
// bytes come back untouched, sharing the caller's allocation.
#[tokio::test]
async fn a_whole_buffer_passes_through_buffering_zero_copy() {
    let data = Bytes::from_static(b"zero-copy");
    let mut rx = data.clone();
    let out = buffer_stream(&mut rx, data.len() as u64).await.unwrap();
    assert_eq!(out, data);
    assert_eq!(out.as_ptr(), data.as_ptr());
}

#[tokio::test]
async fn buffer_accepts_an_exact_stream() {
    let mut rx = feed(vec![
        Segment::Next(Bytes::from_static(b"he")),
        Segment::Final(Bytes::from_static(b"llo")),
    ])
    .await;
    assert_eq!(buffer_stream(&mut rx, 5).await.unwrap().as_ref(), b"hello");
}

#[tokio::test]
async fn buffer_rejects_an_underrun() {
    let mut rx = feed(vec![Segment::Final(Bytes::from_static(b"abc"))]).await;
    assert!(matches!(
        buffer_stream(&mut rx, 5).await,
        Err(BufferError::Underrun {
            size: 3,
            expected: 5
        })
    ));
}

#[tokio::test]
async fn buffer_rejects_an_overrun() {
    let mut rx = feed(vec![Segment::Final(Bytes::from_static(b"abcdef"))]).await;
    assert!(matches!(
        buffer_stream(&mut rx, 5).await,
        Err(BufferError::TooLarge { size: 6, max: 5 })
    ));
}

#[tokio::test]
async fn buffer_rejects_a_truncated_stream() {
    let (tx, mut rx) = hardy_async::channel::bounded(1);
    hardy_async::channel::Sender::send(&tx, Segment::Next(Bytes::from_static(b"part")))
        .await
        .unwrap();
    drop(tx); // no Final: the producer died mid-stream
    assert!(matches!(
        buffer_stream(&mut rx, 10).await,
        Err(BufferError::Cancelled)
    ));
}
