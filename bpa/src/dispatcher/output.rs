//! The egress door's bytes: a stored bundle's load stream, held until its
//! header region is resident, then replayed under the rebuilt headers.
//!
//! The egress door opens the stored bundle with `Store::load_stream` and
//! pulls it until the bytes held reach the payload's data ([`pull_headers`]).
//! Every header precedes it, since the payload is the last block (RFC 9171
//! §4.1), so every block the output stages edit is resident; a Rewriter reads
//! the payload only while it is. The stages run on one editor over those
//! resident bytes and rebuild once, and a
//! [`RebuiltHeaders`](crate::rebuilt_headers::RebuiltHeaders) over the
//! [`Resident`] stream carries the rebuilt header region to the CLA, then the
//! stored payload block as it stands: the resident part, then the rest of the
//! load stream as it arrives. Nothing on this path flattens the whole bundle,
//! and the payload passes through untouched.

use super::*;
use crate::stream::{Receiver, RecvError, Segment};

/// A stored bundle as the egress door holds it: `bytes`, a prefix of the
/// stored bundle that covers every header, and the rest of the load stream,
/// held until its `Final` is in hand.
///
/// As a [`Receiver`] it yields the stored bundle from its first byte: the
/// held prefix as one segment, then the rest of the load stream.
pub struct Resident {
    pub bytes: Bytes,
    rest: Option<Box<dyn Receiver<Segment>>>,
    // The held prefix has been yielded.
    replayed: bool,
}

#[async_trait]
impl Receiver<Segment> for Resident {
    async fn recv(&mut self) -> Result<Segment, RecvError> {
        if !self.replayed {
            self.replayed = true;
            let bytes = self.bytes.clone();
            return Ok(match self.rest {
                Some(_) => Segment::Next(bytes),
                None => Segment::Final(bytes),
            });
        }
        let Some(rest) = self.rest.as_mut() else {
            return Err(RecvError);
        };
        let segment = rest.recv().await?;
        if matches!(segment, Segment::Final(_)) {
            self.rest = None;
        }
        Ok(segment)
    }
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
        replayed: false,
    }
}

#[cfg(test)]
mod tests {
    use hardy_bpv7::{
        block::Type,
        builder::Builder,
        creation_timestamp::CreationTimestamp,
        editor::{Chunk, Editor},
        reader::{Availability, Reader},
    };
    use hardy_cbor::encode::emit;

    use super::*;
    use crate::rebuilt_headers::RebuiltHeaders;

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
        let mut editor = Editor::new(bundle, source);
        editor
            .insert_block(Type::PreviousNode)
            .unwrap()
            .with_data(emit(&node).0.into())
            .rebuild();
        editor.rebuild_bundle().unwrap()
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

    // The rebuild's segments over the bytes `pull_headers` held, as the
    // egress door streams them.
    async fn output(bundle: &Bpv7Bundle, mut resident: Resident) -> (u64, Vec<Segment>) {
        let (rebuilt, chunks) = rebuild(bundle, &resident.bytes);
        let total_len = rebuilt.encoded_len();
        let head = resident.bytes.clone();
        let mut stream = RebuiltHeaders::new(chunks, &head, &mut resident);
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

    // The rebuilt header region travels as one segment, and the kept payload
    // block as a slice of the stored allocation.
    #[tokio::test]
    async fn the_payload_shares_the_stored_allocation() {
        let (bundle, data) = stored(b"zero-copy payload");
        let resident = pull_headers(&bundle, Box::new(data.clone())).await;
        let (_, segments) = output(&bundle, resident).await;
        let [Segment::Next(head), Segment::Final(payload)] = segments.as_slice() else {
            panic!("the rebuilt headers, then the payload block: {segments:?}");
        };
        let stored_range = data.as_ptr_range();
        assert!(
            !stored_range.contains(&head.as_ptr()),
            "the head is rebuilt"
        );
        assert!(stored_range.contains(&payload.as_ptr()));
    }

    // The attempt's editor, as a Rewriter reads through it, sees the payload
    // only while it is resident.
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
            let (_, availability) = Reader::block(&editor, 1).unwrap();
            if resident_payload {
                assert!(
                    matches!(availability, Availability::Available(_)),
                    "cut at {splits:?}: the payload is resident"
                );
            } else {
                assert!(
                    matches!(availability, Availability::NotResident),
                    "cut at {splits:?}: the payload is past the resident end"
                );
            }
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
