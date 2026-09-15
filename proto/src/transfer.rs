// The sequencing of an outgoing transfer, shared by both ends: one
// segment stream re-framed as wire chunks, until the far end has the
// whole bundle. What each writer races, and what it owes the wire when
// a transfer ends early, lives with its side.

use core::ops::ControlFlow;

use hardy_bpa::stream::Segment;

use crate::CHUNK_SIZE;

// Re-frames one segment as [`CHUNK_SIZE`]-bounded wire chunks. An empty
// final segment still yields its `Final` marker; an empty intermediate
// segment yields nothing.
fn chunks(segment: Segment) -> impl Iterator<Item = Segment> {
    let (mut bytes, last) = match segment {
        Segment::Next(bytes) => (bytes, false),
        Segment::Final(bytes) => (bytes, true),
    };
    let mut done = bytes.is_empty() && !last;
    core::iter::from_fn(move || {
        if done {
            return None;
        }
        let slice = bytes.split_to(bytes.len().min(CHUNK_SIZE));
        done = bytes.is_empty();
        Some(if last && done {
            Segment::Final(slice)
        } else {
            Segment::Next(slice)
        })
    })
}

// The outgoing half of a transfer: the two ends from which
// [`write_all`](Writer::write_all) drives the whole thing.
// Implementations differ in only two things: what each await has to
// race, and what an ending short of the final segment means, which is
// `Break`. Terminal wire messages are the implementation's business.
pub(crate) trait Writer {
    // What a transfer that ended before its final segment yields.
    type Break;

    // The next segment of the transfer.
    async fn next(&mut self) -> ControlFlow<Self::Break, Segment>;

    // One chunk onto the wire, once there is room for it. A writer
    // spends most of its life here.
    async fn write(&mut self, chunk: Segment) -> ControlFlow<Self::Break>;

    // Every segment, re-framed by [`chunks`], until the final segment's
    // last chunk is on the wire: `Continue` means the far end has the
    // whole bundle, `Break` means it does not.
    //
    // By value, so the returned future owns the writer: a caller that
    // races this and loses drops the writer with the future, releasing
    // the segment stream there and then, so an abandonment reaches the
    // producer at once.
    async fn write_all(mut self) -> ControlFlow<Self::Break>
    where
        Self: Sized,
    {
        loop {
            let segment = match self.next().await {
                ControlFlow::Continue(segment) => segment,
                ControlFlow::Break(ended) => return ControlFlow::Break(ended),
            };
            let last = matches!(segment, Segment::Final(_));
            for chunk in chunks(segment) {
                if let ControlFlow::Break(ended) = self.write(chunk).await {
                    return ControlFlow::Break(ended);
                }
            }
            if last {
                return ControlFlow::Continue(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpa::Bytes;

    use super::*;

    // What each chunk carries, and whether it is the one that ends the
    // transfer.
    fn frame(segment: Segment) -> Vec<(Bytes, bool)> {
        chunks(segment)
            .map(|chunk| match chunk {
                Segment::Next(bytes) => (bytes, false),
                Segment::Final(bytes) => (bytes, true),
            })
            .collect()
    }

    // `len` distinguishable bytes, so a re-framing that reorders or drops
    // a slice fails the round-trip.
    fn payload(len: usize) -> Bytes {
        Bytes::from_iter((0..len).map(|i| (i % 251) as u8))
    }

    #[test]
    fn an_empty_intermediate_segment_yields_nothing() {
        assert_eq!(frame(Segment::Next(Bytes::new())), []);
    }

    #[test]
    fn an_empty_final_segment_still_ends_the_transfer() {
        assert_eq!(frame(Segment::Final(Bytes::new())), [(Bytes::new(), true)]);
    }

    #[test]
    fn a_segment_within_the_bound_is_one_chunk() {
        let bytes = payload(CHUNK_SIZE - 1);
        assert_eq!(
            frame(Segment::Next(bytes.clone())),
            [(bytes.clone(), false)]
        );
        assert_eq!(frame(Segment::Final(bytes.clone())), [(bytes, true)]);
    }

    #[test]
    fn a_segment_of_exactly_the_bound_is_one_chunk() {
        let bytes = payload(CHUNK_SIZE);
        assert_eq!(
            frame(Segment::Next(bytes.clone())),
            [(bytes.clone(), false)]
        );
        assert_eq!(frame(Segment::Final(bytes.clone())), [(bytes, true)]);
    }

    #[test]
    fn only_the_last_chunk_of_a_final_segment_is_final() {
        let bytes = payload(2 * CHUNK_SIZE + 7);
        let frames = frame(Segment::Final(bytes.clone()));
        assert_eq!(
            frames
                .iter()
                .map(|(chunk, last)| (chunk.len(), *last))
                .collect::<Vec<_>>(),
            [(CHUNK_SIZE, false), (CHUNK_SIZE, false), (7, true)]
        );
        // The re-framing is a split, not a rewrite.
        assert_eq!(
            frames
                .into_iter()
                .flat_map(|(chunk, _)| chunk)
                .collect::<Bytes>(),
            bytes
        );
    }

    #[test]
    fn no_chunk_of_an_intermediate_segment_ends_the_transfer() {
        let bytes = payload(2 * CHUNK_SIZE);
        let frames = frame(Segment::Next(bytes.clone()));
        assert_eq!(
            frames
                .iter()
                .map(|(chunk, last)| (chunk.len(), *last))
                .collect::<Vec<_>>(),
            [(CHUNK_SIZE, false), (CHUNK_SIZE, false)]
        );
        assert_eq!(
            frames
                .into_iter()
                .flat_map(|(chunk, _)| chunk)
                .collect::<Bytes>(),
            bytes
        );
    }
}
