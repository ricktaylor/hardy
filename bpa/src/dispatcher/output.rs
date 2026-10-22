//! The output doors' bytes: a stored bundle's load stream, held until its
//! header region is resident, and the rebuilt bundle handed on as segments.
//!
//! An output door opens the stored bundle with `Store::load_stream` and pulls
//! it until the bytes held reach the payload's data ([`pull_headers`]). Every
//! header precedes it, since the payload is the last block (RFC 9171 §4.1),
//! so every block the output stages edit is resident; a Rewriter reads the
//! payload only while it is. The stages run on one editor over those
//! resident bytes and rebuild once, and the rebuild's chunks travel as
//! segments ([`ChunkReceiver`]): an edited block's new bytes, a kept block's
//! resident extent zero-copy, and after a kept payload the rest of the stored
//! stream. Nothing on this path is flattened, and the payload passes through
//! untouched.

use alloc::collections::VecDeque;

use hardy_bpv7::{bundle::Bundle as Bpv7Bundle, editor::Chunk};

use super::*;
use crate::stream::{Receiver, RecvError, Segment};

// The outer CBOR indefinite-length array's head and break (RFC 9171 §4.1):
// a rebuild's chunks are the blocks between them.
const ARRAY_HEAD: &[u8] = &[0x9F];
const ARRAY_BREAK: &[u8] = &[0xFF];

/// A stored bundle as an output door holds it: the bytes pulled so far, a
/// prefix of the stored bundle that covers every header, and the rest of the
/// load stream, `None` once its `Final` is in hand.
pub struct Resident {
    pub bytes: Bytes,
    pub rest: Option<Box<dyn Receiver<Segment>>>,
}

/// Pulls `stream` until the bytes held reach `bundle`'s payload data, or the
/// stream's `Final`. When the first segment covers the header region, as a
/// whole-buffer load's one `Final` does, it is held as-is; only a header
/// region split across segments is concatenated.
///
/// # Panics
///
/// Panics if the stream ends before the header region is resident: the store
/// holds only bundles validated at a door or built by the BPA, so a short
/// load is storage corruption.
pub async fn pull_headers(bundle: &Bpv7Bundle, mut stream: Box<dyn Receiver<Segment>>) -> Resident {
    let payload_start = bundle
        .blocks
        .get(&1)
        .trace_expect("A stored bundle has a payload block")
        .payload_range()
        .start;

    let mut first: Option<Bytes> = None;
    let mut concat: Option<BytesMut> = None;
    let last = loop {
        let (data, last) = match stream
            .recv()
            .await
            .trace_expect("The bundle's load stream ended before its headers")
        {
            Segment::Next(data) => (data, false),
            Segment::Final(data) => (data, true),
        };

        let held = if let Some(current) = concat.as_mut() {
            current.extend_from_slice(&data);
            current.len()
        } else if let Some(head) = first.take() {
            let mut current = BytesMut::with_capacity(head.len() + data.len());
            current.extend_from_slice(&head);
            current.extend_from_slice(&data);
            let held = current.len();
            concat = Some(current);
            held
        } else {
            let held = data.len();
            first = Some(data);
            held
        };

        if last || held as u64 >= payload_start {
            break last;
        }
    };

    let bytes = match (first, concat) {
        (Some(bytes), None) => bytes,
        (None, Some(buffer)) => buffer.freeze(),
        _ => unreachable!("exactly one accumulator holds the pulled bytes"),
    };
    Resident {
        bytes: Some(bytes)
            .filter(|bytes| bytes.len() as u64 >= payload_start)
            .trace_expect("The stored bundle ends before its headers"),
        rest: (!last).then_some(stream),
    }
}

/// A rebuilt bundle as a segment stream: the outer array's head, then each of
/// the rebuild's chunks as a segment, then the stored bytes after the payload.
///
/// A [`Chunk::New`] travels as its bytes, and a [`Chunk::Unchanged`] as a
/// zero-copy slice of the resident bytes. Only the payload, the last chunk,
/// can run past the resident end. A kept payload's segment runs from its
/// start to the resident end, which in the stored bytes is followed by the
/// outer array's break, and the rest of the load stream follows it, ending
/// with the stream's `Final`. A rebuilt payload is wholly resident, so its
/// segment is followed by the break and the rest of the stream is not read.
pub struct ChunkReceiver {
    pieces: VecDeque<Bytes>,
    rest: Option<Box<dyn Receiver<Segment>>>,
    yielded: u64,
    total_len: u64,
}

impl ChunkReceiver {
    /// The segment stream of the rebuild whose `chunks` index `resident`'s
    /// bytes. `total_len` is the rebuilt bundle's encoded length, which debug
    /// builds check the yielded bytes against.
    ///
    /// # Panics
    ///
    /// Panics if a chunk other than the last lies outside the resident bytes:
    /// the header region is resident by [`pull_headers`].
    pub fn new(chunks: Vec<Chunk>, resident: Resident, total_len: u64) -> Self {
        let Resident { bytes, mut rest } = resident;
        let mut pieces = VecDeque::with_capacity(chunks.len() + 2);
        pieces.push_back(Bytes::from_static(ARRAY_HEAD));
        let count = chunks.len();
        for (index, chunk) in chunks.into_iter().enumerate() {
            let last = index + 1 == count;
            match chunk {
                Chunk::Unchanged(extent) if last => pieces.push_back(bytes.slice(extent.start..)),
                Chunk::Unchanged(extent) => pieces.push_back(bytes.slice(extent)),
                Chunk::New(data) => {
                    pieces.push_back(Bytes::from(data));
                    if last {
                        pieces.push_back(Bytes::from_static(ARRAY_BREAK));
                        rest = None;
                    }
                }
            }
        }
        Self {
            pieces,
            rest,
            yielded: 0,
            total_len,
        }
    }
}

#[async_trait]
impl Receiver<Segment> for ChunkReceiver {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        let segment = if let Some(piece) = self.pieces.pop_front() {
            if self.pieces.is_empty() && self.rest.is_none() {
                Segment::Final(piece)
            } else {
                Segment::Next(piece)
            }
        } else if let Some(rest) = self.rest.as_mut() {
            let segment = rest.recv().await?;
            if matches!(segment, Segment::Final(_)) {
                self.rest = None;
            }
            segment
        } else {
            // Drained past the Final, like a whole buffer: consumers stop at
            // the first Final.
            return Ok(Segment::Final(Bytes::new()));
        };
        let (Segment::Next(data) | Segment::Final(data)) = &segment;
        self.yielded += data.len() as u64;
        debug_assert!(
            matches!(segment, Segment::Next(_)) || self.yielded == self.total_len,
            "the rebuilt bundle's segments total its encoded length"
        );
        Ok(segment)
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::{
        block::Type,
        builder::Builder,
        creation_timestamp::CreationTimestamp,
        editor::Editor,
        reader::{Availability, Reader},
    };

    use super::*;

    // A stored bundle carrying `payload`, with its block index.
    fn stored(payload: &[u8]) -> (Bpv7Bundle, Bytes) {
        let (bundle, data) =
            Builder::new("ipn:0.2.1".parse().unwrap(), "ipn:0.3.2".parse().unwrap())
                .with_payload(payload.to_vec().into())
                .build(CreationTimestamp::now())
                .unwrap();
        (bundle, Bytes::from(data))
    }

    fn payload_start(bundle: &Bpv7Bundle) -> usize {
        usize::try_from(bundle.blocks[&1].payload_range().start).unwrap()
    }

    // The stored bundle's load stream, cut at `splits`, ending in its Final.
    async fn load(data: &Bytes, splits: &[usize]) -> Box<dyn Receiver<Segment>> {
        let (tx, rx) = hardy_async::channel::bounded(splits.len() + 1);
        let mut start = 0;
        for &split in splits {
            tx.send(Segment::Next(data.slice(start..split)))
                .await
                .unwrap();
            start = split;
        }
        tx.send(Segment::Final(data.slice(start..))).await.unwrap();
        Box::new(rx)
    }

    // An output attempt's one rebuild: a Previous Node insert, so the chunks
    // mix new bytes with kept extents.
    fn rebuild(bundle: &Bpv7Bundle, source: &[u8]) -> (Bpv7Bundle, Vec<Chunk>) {
        let node: Eid = "ipn:0.9.0".parse().unwrap();
        Editor::new(bundle, source)
            .insert_block(Type::PreviousNode)
            .map_err(|(_, e)| e)
            .unwrap()
            .with_data(hardy_cbor::encode::emit(&node).0.into())
            .rebuild()
            .rebuild_bundle()
            .unwrap()
    }

    // Pulls segments until the Final, inclusive.
    async fn drain(stream: &mut dyn Receiver<Segment>) -> Vec<Segment> {
        let mut segments = Vec::new();
        loop {
            let segment = stream.recv().await.unwrap();
            let last = matches!(segment, Segment::Final(_));
            segments.push(segment);
            if last {
                return segments;
            }
        }
    }

    fn concat(segments: &[Segment]) -> Vec<u8> {
        segments
            .iter()
            .flat_map(|(Segment::Next(data) | Segment::Final(data))| data.iter().copied())
            .collect()
    }

    // The rebuild's segments over the bytes `pull_headers` held.
    async fn output(bundle: &Bpv7Bundle, resident: Resident) -> (u64, Vec<Segment>) {
        let (rebuilt, chunks) = rebuild(bundle, &resident.bytes);
        let total_len = rebuilt.encoded_len();
        let mut stream = ChunkReceiver::new(chunks, resident, total_len);
        (total_len, drain(&mut stream).await)
    }

    #[tokio::test]
    async fn a_whole_buffer_load_is_held_as_is() {
        let (bundle, data) = stored(b"held as loaded");
        let resident = pull_headers(&bundle, Box::new(data.clone())).await;
        assert_eq!(resident.bytes.as_ptr(), data.as_ptr());
        assert_eq!(resident.bytes.len(), data.len());
        assert!(resident.rest.is_none());
    }

    // Wherever the load is cut, the pull holds through the headers and the
    // segments carry the bytes the flattened rebuild of the whole bundle
    // would: inside the header region (the head concatenated), exactly at
    // the payload's data, and inside it.
    #[tokio::test]
    async fn every_cut_streams_the_flattened_rebuild() {
        let (bundle, data) = stored(&[0x5A; 64]);
        let start = payload_start(&bundle);
        let (_, whole) = rebuild(&bundle, &data);
        let expected = Chunk::flatten(whole, &data);

        for (splits, held, shared) in [
            (vec![], data.len(), true),
            (vec![10, start + 10], start + 10, false),
            (vec![start], start, true),
            (vec![start + 3], start + 3, true),
        ] {
            let resident = pull_headers(&bundle, load(&data, &splits).await).await;
            assert_eq!(resident.bytes.len(), held, "cut at {splits:?}");
            assert_eq!(
                resident.bytes.as_ptr() == data.as_ptr(),
                shared,
                "cut at {splits:?}: only a split header region is concatenated"
            );
            assert_eq!(resident.rest.is_some(), !splits.is_empty());

            let (total_len, segments) = output(&bundle, resident).await;
            assert_eq!(concat(&segments), &*expected, "cut at {splits:?}");
            assert_eq!(total_len, expected.len() as u64);
        }
    }

    // Each kept chunk, the payload's included, travels as a slice of the
    // stored allocation.
    #[tokio::test]
    async fn kept_chunks_share_the_stored_allocation() {
        let (bundle, data) = stored(b"zero-copy payload");
        let resident = pull_headers(&bundle, Box::new(data.clone())).await;
        let (rebuilt, chunks) = rebuild(&bundle, &resident.bytes);
        let kept: Vec<bool> = chunks
            .iter()
            .map(|chunk| matches!(chunk, Chunk::Unchanged(_)))
            .collect();
        assert!(kept.contains(&false) && kept.last() == Some(&true));

        let mut stream = ChunkReceiver::new(chunks, resident, rebuilt.encoded_len());
        let segments = drain(&mut stream).await;
        // The array head, then one segment per chunk.
        assert_eq!(segments.len(), kept.len() + 1);
        let stored_range = data.as_ptr_range();
        for (segment, kept) in segments[1..].iter().zip(kept) {
            let (Segment::Next(bytes) | Segment::Final(bytes)) = segment;
            assert_eq!(stored_range.contains(&bytes.as_ptr()), kept);
        }
    }

    // A reader over the attempt's editor, as a Rewriter reads, sees the
    // payload only while it is resident.
    #[tokio::test]
    async fn a_payload_past_the_resident_end_reads_not_resident() {
        let (bundle, data) = stored(&[0xA5; 64]);
        let start = payload_start(&bundle);
        for (splits, resident_payload) in [
            (vec![], true),
            (vec![start], false),
            (vec![start + 3], false),
        ] {
            let resident = pull_headers(&bundle, load(&data, &splits).await).await;
            let editor = Editor::new(&bundle, &resident.bytes);
            let view = editor.staged_view().unwrap();
            let (_, availability) = view.block(1).unwrap();
            assert_eq!(
                matches!(availability, Availability::Available(_)),
                resident_payload,
                "cut at {splits:?}"
            );
        }
    }

    #[tokio::test]
    async fn an_empty_payload_streams() {
        let (bundle, data) = stored(b"");
        let (_, whole) = rebuild(&bundle, &data);
        let expected = Chunk::flatten(whole, &data);
        let resident = pull_headers(&bundle, Box::new(data.clone())).await;
        let (total_len, segments) = output(&bundle, resident).await;
        assert_eq!(concat(&segments), &*expected);
        assert_eq!(total_len, expected.len() as u64);
    }

    // A rebuilt last chunk is wholly in hand: the receiver closes the array
    // itself and reads none of the stored stream after it.
    #[tokio::test]
    async fn a_rebuilt_last_chunk_closes_the_array() {
        let (bundle, data) = stored(&[0x3C; 64]);
        let start = payload_start(&bundle);
        let (_, whole) = rebuild(&bundle, &data);
        let expected = Chunk::flatten(whole, &data);
        let (rebuilt, mut chunks) = rebuild(&bundle, &data);
        let Some(Chunk::Unchanged(extent)) = chunks.pop() else {
            panic!("the payload is the rebuild's last chunk, and kept");
        };
        chunks.push(Chunk::New(data[extent].into()));

        let resident = pull_headers(&bundle, load(&data, &[start + 3]).await).await;
        let mut stream = ChunkReceiver::new(chunks, resident, rebuilt.encoded_len());
        assert_eq!(concat(&drain(&mut stream).await), &*expected);
    }

    #[tokio::test]
    #[should_panic(expected = "ends before its headers")]
    async fn a_stored_bundle_short_of_its_headers_is_fatal() {
        let (bundle, data) = stored(b"cut short");
        pull_headers(&bundle, Box::new(data.slice(..10))).await;
    }

    #[tokio::test]
    #[should_panic(expected = "ended before its headers")]
    async fn a_load_lost_before_the_headers_is_fatal() {
        let (bundle, data) = stored(b"lost");
        let (tx, rx) = hardy_async::channel::bounded(1);
        tx.send(Segment::Next(data.slice(..10))).await.unwrap();
        drop(tx);
        pull_headers(&bundle, Box::new(rx)).await;
    }
}
