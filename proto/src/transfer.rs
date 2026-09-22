// Outgoing transfers: a segment stream re-framed as `CHUNK_SIZE`-bounded
// wire chunks.

use hardy_bpa::stream::Segment;

use crate::CHUNK_SIZE;

// Re-frames one segment as `CHUNK_SIZE`-bounded chunks. An empty final
// segment still yields its `Final` marker; an empty intermediate
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

// One outgoing transfer: a segment source and a chunk sink, driven by
// `write_all`.
pub(crate) trait Writer {
    type Error;

    async fn next(&mut self) -> Result<Segment, Self::Error>;

    // Writes one chunk, waiting until there is room for it.
    async fn write(&mut self, chunk: Segment) -> Result<(), Self::Error>;

    // Writes every segment, re-framed by `chunks`, until the final
    // chunk is written. Consumes the writer so a caller that loses a
    // `select!` race against this future drops it, releasing the
    // segment stream.
    async fn write_all(mut self) -> Result<(), Self::Error>
    where
        Self: Sized,
    {
        loop {
            let segment = self.next().await?;
            let last = matches!(segment, Segment::Final(_));
            for chunk in chunks(segment) {
                self.write(chunk).await?;
            }
            if last {
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpa::Bytes;

    use super::*;

    // Each chunk's bytes, and whether the chunk is `Final`.
    fn frame(segment: Segment) -> Vec<(Bytes, bool)> {
        chunks(segment)
            .map(|chunk| match chunk {
                Segment::Next(bytes) => (bytes, false),
                Segment::Final(bytes) => (bytes, true),
            })
            .collect()
    }

    // `len` distinguishable bytes.
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
        // Concatenating the chunks reproduces the original bytes.
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
